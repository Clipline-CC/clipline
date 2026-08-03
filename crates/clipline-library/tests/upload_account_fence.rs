use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use chrono::Utc;
use clipline_library::ports::CloudCredential;
use clipline_library::protocol::{
    sha256_hex, CloudApiBase, CreateUploadRequest, UploadProgressResponse,
};
use clipline_library::{
    client_clip_id_for_payload, ActiveFileRegistry, CloudAccountGeneration, CloudAccountKey,
    LocalClipId, LocalLibraryRepository, PreparedUploadPayload, ReadyUpload,
    StandardRepositoryFileSystem, UploadAccountFence, UploadAccountOwner, UploadCancellation,
    UploadDeletePermit, UploadDeletionPort, UploadEventPort, UploadEventPortError,
    UploadGeneration, UploadIntent, UploadJobOutcome, UploadPhase, UploadPreparationPort,
    UploadRecord, UploadRecordCursor, UploadRecordError, UploadRecordErrorKind, UploadRecordPort,
    UploadRemoteOutcome, UploadRemotePort, UploadService, UploadServiceEvent, UploadStartRequest,
    UploadTransportPort, UploadWorkError, ValidatedClipPath,
};
use clipline_test_utils::TestDir;

fn key(record: &UploadRecord) -> (String, u64, String) {
    (
        record.token.account_key.as_str().into(),
        record.token.account_generation.get(),
        record.token.local_clip_id.as_str().into(),
    )
}

#[derive(Default)]
struct Accounts(Mutex<Option<UploadAccountOwner>>);

impl Accounts {
    fn set(&self, owner: UploadAccountOwner) {
        *self.0.lock().unwrap() = Some(owner);
    }
}

impl UploadAccountFence for Accounts {
    fn is_current(&self, owner: &UploadAccountOwner) -> bool {
        self.0.lock().unwrap().as_ref() == Some(owner)
    }
}

#[derive(Default)]
struct Records(
    Mutex<HashMap<(String, u64, String), UploadRecordCursor>>,
    AtomicU64,
);

impl Records {
    fn current(&self, token: &clipline_library::DurableUploadToken) -> UploadRecordCursor {
        self.0
            .lock()
            .unwrap()
            .get(&(
                token.account_key.as_str().into(),
                token.account_generation.get(),
                token.local_clip_id.as_str().into(),
            ))
            .unwrap()
            .clone()
    }

    fn force_advance(&self, expected: &UploadRecordCursor, error: &str) -> UploadRecordCursor {
        let mut records = self.0.lock().unwrap();
        let mut record = expected.record.clone();
        record.error = Some(error.into());
        let next = UploadRecordCursor {
            revision: expected.revision + 1,
            record,
        };
        records.insert(key(&expected.record), next.clone());
        next
    }

    fn snapshot(&self) -> HashMap<(String, u64, String), UploadRecordCursor> {
        self.0.lock().unwrap().clone()
    }
}

impl UploadRecordPort for Records {
    fn allocate_generation(
        &self,
        _owner: &UploadAccountOwner,
        _local_clip_id: &LocalClipId,
        _source: &ValidatedClipPath,
    ) -> Result<UploadGeneration, UploadRecordError> {
        let generation = self
            .1
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                value.checked_add(1)
            })
            .map_err(|_| {
                UploadRecordError::new(
                    UploadRecordErrorKind::Persistence,
                    "upload generation exhausted",
                )
            })?
            + 1;
        Ok(UploadGeneration::new(generation))
    }

    fn load(
        &self,
        owner: &UploadAccountOwner,
        local_clip_id: &LocalClipId,
    ) -> Result<Option<UploadRecordCursor>, UploadRecordError> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .get(&(
                owner.account_key.as_str().into(),
                owner.account_generation.get(),
                local_clip_id.as_str().into(),
            ))
            .cloned())
    }

    fn admit(&self, record: UploadRecord) -> Result<UploadRecordCursor, UploadRecordError> {
        let mut records = self.0.lock().unwrap();
        let record_key = key(&record);
        if records
            .get(&record_key)
            .is_some_and(|record| !record.record.phase.is_terminal())
        {
            return Err(UploadRecordError::new(
                UploadRecordErrorKind::Contended,
                "active upload",
            ));
        }
        let cursor = UploadRecordCursor {
            revision: records
                .get(&record_key)
                .map_or(1, |record| record.revision + 1),
            record,
        };
        records.insert(record_key, cursor.clone());
        Ok(cursor)
    }

    fn compare_exchange(
        &self,
        expected: &UploadRecordCursor,
        replacement: UploadRecord,
    ) -> Result<UploadRecordCursor, UploadRecordError> {
        let mut records = self.0.lock().unwrap();
        let record_key = key(&expected.record);
        if records.get(&record_key) != Some(expected)
            || key(&replacement) != record_key
            || replacement.token != expected.record.token
        {
            return Err(UploadRecordError::new(
                UploadRecordErrorKind::Superseded,
                "stale upload record",
            ));
        }
        let cursor = UploadRecordCursor {
            revision: expected.revision + 1,
            record: replacement,
        };
        records.insert(record_key, cursor.clone());
        Ok(cursor)
    }

    fn remove_exact(&self, expected: &UploadRecordCursor) -> Result<(), UploadRecordError> {
        let mut records = self.0.lock().unwrap();
        let record_key = key(&expected.record);
        if records.get(&record_key) != Some(expected) {
            return Err(UploadRecordError::new(
                UploadRecordErrorKind::Superseded,
                "stale upload record",
            ));
        }
        records.remove(&record_key);
        Ok(())
    }
}

#[derive(Default)]
struct Events(Mutex<Vec<UploadServiceEvent>>);

impl UploadEventPort for Events {
    fn try_publish(&self, event: UploadServiceEvent) -> Result<(), UploadEventPortError> {
        self.0.lock().unwrap().push(event);
        Ok(())
    }
}

struct Preparation;

impl UploadPreparationPort for Preparation {
    fn prepare<'a>(
        &'a self,
        source: &'a clipline_library::UploadSourceLease,
        intent: &'a UploadIntent,
        _cancellation: &'a UploadCancellation,
    ) -> clipline_library::UploadFuture<'a, Result<PreparedUploadPayload, UploadWorkError>> {
        Box::pin(async move {
            let bytes = std::fs::read(source.canonical_path())
                .map_err(|error| UploadWorkError::failed(error.to_string()))?;
            let checksum = sha256_hex(&bytes);
            let client = client_clip_id_for_payload(&source.token().local_clip_id, &checksum)
                .map_err(|error| UploadWorkError::failed(error.to_string()))?;
            let request = CreateUploadRequest {
                client_clip_id: Some(client.as_str().into()),
                title: "clip".into(),
                description: None,
                game_name: None,
                game_id: None,
                game_executable: None,
                source_type: Some("manual".into()),
                recorded_at: None,
                duration_ms: Some(1_000),
                file_size_bytes: bytes.len() as u64,
                checksum_sha256: checksum,
                container: "mp4".into(),
                video_codec: Some("h264".into()),
                audio_codec: Some("opus".into()),
                width: Some(640),
                height: Some(360),
                fps: Some(60.0),
                visibility: Some(intent.visibility.clone()),
                markers: None,
            };
            PreparedUploadPayload::original(source, request, None, client)
        })
    }
}

/// Account A intentionally ignores cancellation to reproduce a delayed HTTP
/// completion arriving after disconnect/reconnect.
struct DelayedTransport {
    delay_a: AtomicBool,
    a_entered: tokio::sync::Notify,
    release_a: tokio::sync::Notify,
}

impl DelayedTransport {
    fn new() -> Self {
        Self {
            delay_a: AtomicBool::new(true),
            a_entered: tokio::sync::Notify::new(),
            release_a: tokio::sync::Notify::new(),
        }
    }
}

impl UploadTransportPort for DelayedTransport {
    fn upload<'a>(
        &'a self,
        endpoint: &'a clipline_library::UploadEndpoint,
        payload: &'a PreparedUploadPayload,
        _cancellation: &'a UploadCancellation,
        on_progress: &'a mut (dyn FnMut(&UploadProgressResponse) + Send),
    ) -> clipline_library::UploadFuture<'a, Result<UploadProgressResponse, UploadWorkError>> {
        Box::pin(async move {
            if endpoint.owner().account_key.as_str() == "account-a"
                && self.delay_a.load(Ordering::Acquire)
            {
                self.a_entered.notify_waiters();
                self.release_a.notified().await;
            }
            let progress = UploadProgressResponse {
                upload_id: format!("upload-{}", endpoint.owner().account_key.as_str()),
                clip_id: format!("remote-{}", endpoint.owner().account_key.as_str()),
                mode: "single_put".into(),
                status: "completed".into(),
                file_size_bytes: payload.request().file_size_bytes,
                part_size_bytes: payload.request().file_size_bytes,
                received_size_bytes: payload.request().file_size_bytes,
                total_parts: 1,
                received_part_count: 1,
                missing_part_count: 0,
                next_part_number: None,
                progress_basis_points: 10_000,
                failure_reason: None,
                recovery_action: None,
                expires_at: Utc::now(),
                received_parts: vec![1],
                missing_parts: vec![],
            };
            on_progress(&progress);
            Ok(progress)
        })
    }
}

struct Remote;

impl UploadRemotePort for Remote {
    fn wait_until_ready<'a>(
        &'a self,
        _endpoint: &'a clipline_library::UploadEndpoint,
        remote_clip_id: &'a str,
        visibility: &'a str,
        _cancellation: &'a UploadCancellation,
    ) -> clipline_library::UploadFuture<'a, Result<UploadRemoteOutcome, UploadWorkError>> {
        Box::pin(async move {
            Ok(UploadRemoteOutcome::Ready(ReadyUpload {
                remote_clip_id: remote_clip_id.into(),
                visibility: visibility.into(),
                remote_url: None,
            }))
        })
    }

    fn probe_media<'a>(
        &'a self,
        _endpoint: &'a clipline_library::UploadEndpoint,
        _remote_clip_id: &'a str,
        _cancellation: &'a UploadCancellation,
    ) -> clipline_library::UploadFuture<'a, Result<(), UploadWorkError>> {
        Box::pin(async { Ok(()) })
    }
}

struct NoDeletion;

impl UploadDeletionPort for NoDeletion {
    fn delete_local(&self, _permit: &UploadDeletePermit) -> Result<(), UploadWorkError> {
        panic!("delete-after-upload is disabled in account fence tests")
    }
}

struct Harness {
    _directory: TestDir,
    source: ValidatedClipPath,
    accounts: Arc<Accounts>,
    records: Arc<Records>,
    events: Arc<Events>,
    transport: Arc<DelayedTransport>,
    service: UploadService,
}

impl Harness {
    fn new(name: &str) -> Self {
        let directory = TestDir::new("clipline-upload-account", name);
        let root = directory.path().join("media");
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("clip.mp4");
        std::fs::write(&path, b"account fence payload").unwrap();
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
        let accounts = Arc::new(Accounts::default());
        accounts.set(owner("account-a", 1));
        let records = Arc::new(Records::default());
        let events = Arc::new(Events::default());
        let transport = Arc::new(DelayedTransport::new());
        let service = UploadService::new(
            registry,
            accounts.clone(),
            Arc::new(Preparation),
            transport.clone(),
            Arc::new(Remote),
            Arc::new(NoDeletion),
            records.clone(),
            events.clone(),
        );
        Self {
            _directory: directory,
            source,
            accounts,
            records,
            events,
            transport,
            service,
        }
    }

    fn request(&self, account: &str, generation: u64) -> UploadStartRequest {
        UploadStartRequest {
            endpoint: clipline_library::UploadEndpoint::new(
                owner(account, generation),
                CloudApiBase::parse("http://127.0.0.1:1", true).unwrap(),
                CloudCredential::new("secret"),
            ),
            source: self.source.clone(),
            intent: UploadIntent::default(),
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
async fn delayed_account_a_completion_cannot_mutate_or_notify_account_b() {
    let harness = Harness::new("a-completes-after-b");
    let account_a = harness
        .service
        .start(harness.request("account-a", 1))
        .unwrap();
    let token_a = account_a.token().clone();
    harness.transport.a_entered.notified().await;

    let owner_b = owner("account-b", 2);
    harness.accounts.set(owner_b.clone());
    harness.service.account_changed(Some(&owner_b));
    let account_b = harness
        .service
        .start(harness.request("account-b", 2))
        .unwrap();
    let token_b = account_b.token().clone();
    assert_eq!(account_b.wait().await.outcome, UploadJobOutcome::Completed);
    let b_before = harness.records.current(&token_b);
    let b_event_count = harness
        .events
        .0
        .lock()
        .unwrap()
        .iter()
        .filter(|event| event.record.token.account_key.as_str() == "account-b")
        .count();

    harness.transport.delay_a.store(false, Ordering::Release);
    harness.transport.release_a.notify_waiters();
    let stale_completion = account_a.wait().await;
    assert_eq!(stale_completion.outcome, UploadJobOutcome::AccountChanged);
    assert_eq!(stale_completion.record, None);

    assert_eq!(harness.records.current(&token_b), b_before);
    assert_eq!(
        harness
            .events
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|event| event.record.token.account_key.as_str() == "account-b")
            .count(),
        b_event_count
    );
    assert_eq!(
        harness.records.current(&token_a).record.phase,
        UploadPhase::Uploading
    );
}

#[tokio::test]
async fn delayed_status_sync_cas_cannot_replace_a_newer_record() {
    let harness = Harness::new("stale-status-sync");
    harness.transport.delay_a.store(false, Ordering::Release);
    let handle = harness
        .service
        .start(harness.request("account-a", 1))
        .unwrap();
    let token = handle.token().clone();
    assert_eq!(handle.wait().await.outcome, UploadJobOutcome::Completed);
    let stale = harness
        .service
        .status_cursor(&owner("account-a", 1), &token.local_clip_id)
        .unwrap()
        .unwrap();
    assert_eq!(stale, harness.records.current(&token));
    let current = harness.records.force_advance(&stale, "newer status result");
    let mut delayed = stale.record.clone();
    delayed.error = Some("older delayed status".into());

    let error = harness
        .service
        .commit_status_sync(&stale, delayed)
        .unwrap_err();
    assert_eq!(error.kind(), UploadRecordErrorKind::Superseded);
    assert_eq!(harness.records.current(&token), current);
}

#[tokio::test]
async fn status_sync_checks_account_before_record_cas() {
    let harness = Harness::new("status-account-change");
    harness.transport.delay_a.store(false, Ordering::Release);
    let handle = harness
        .service
        .start(harness.request("account-a", 1))
        .unwrap();
    let token = handle.token().clone();
    assert_eq!(handle.wait().await.outcome, UploadJobOutcome::Completed);
    let expected = harness.records.current(&token);
    let before = expected.clone();
    harness.accounts.set(owner("account-b", 2));
    let mut replacement = expected.record.clone();
    replacement.error = Some("delayed A status".into());

    let error = harness
        .service
        .commit_status_sync(&expected, replacement)
        .unwrap_err();
    assert_eq!(error.kind(), UploadRecordErrorKind::AccountChanged);
    assert_eq!(harness.records.current(&token), before);
}

#[tokio::test]
async fn delayed_status_removal_cannot_remove_a_newer_record_or_emit() {
    let harness = Harness::new("stale-status-remove");
    harness.transport.delay_a.store(false, Ordering::Release);
    let handle = harness
        .service
        .start(harness.request("account-a", 1))
        .unwrap();
    let token = handle.token().clone();
    assert_eq!(handle.wait().await.outcome, UploadJobOutcome::Completed);
    let stale = harness.records.current(&token);
    let current = harness.records.force_advance(&stale, "newer status result");
    let before_records = harness.records.snapshot();
    let before_events = harness.events.0.lock().unwrap().clone();

    let error = harness.service.remove_status_sync(&stale).unwrap_err();
    assert_eq!(error.kind(), UploadRecordErrorKind::Superseded);
    assert_eq!(harness.records.current(&token), current);
    assert_eq!(harness.records.snapshot(), before_records);
    assert_eq!(*harness.events.0.lock().unwrap(), before_events);
}

#[tokio::test]
async fn status_removal_checks_account_before_exact_cas_and_emits_nothing() {
    let harness = Harness::new("status-remove-account-change");
    harness.transport.delay_a.store(false, Ordering::Release);
    let handle = harness
        .service
        .start(harness.request("account-a", 1))
        .unwrap();
    let token = handle.token().clone();
    assert_eq!(handle.wait().await.outcome, UploadJobOutcome::Completed);
    let expected = harness.records.current(&token);
    let before_records = harness.records.snapshot();
    let before_events = harness.events.0.lock().unwrap().clone();
    harness.accounts.set(owner("account-b", 2));

    let error = harness.service.remove_status_sync(&expected).unwrap_err();
    assert_eq!(error.kind(), UploadRecordErrorKind::AccountChanged);
    assert_eq!(harness.records.snapshot(), before_records);
    assert_eq!(*harness.events.0.lock().unwrap(), before_events);
}
