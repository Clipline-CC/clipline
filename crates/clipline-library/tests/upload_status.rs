use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use clipline_library::ports::CloudCredential;
use clipline_library::protocol::CloudApiBase;
use clipline_library::{
    ClipPathIdentity, CloudAccountGeneration, CloudAccountKey, DurableUploadToken, LocalClipId,
    RemoteClipObservation, RemoteClipStatus, ReqwestUploadStatusRemote, UploadAccountOwner,
    UploadCancellation, UploadEndpoint, UploadFuture, UploadGeneration, UploadPhase, UploadRecord,
    UploadRecordCursor, UploadRecordError, UploadRecordErrorKind, UploadStatusRecordPort,
    UploadStatusRemotePort, UploadStatusSyncErrorKind, UploadStatusSyncOutcome,
    UploadStatusSyncService, UploadWorkError, MAX_ACTIVE_UPLOAD_STATUS_SYNCS,
    REMOTE_NOT_FOUND_SYNC_MARKER,
};

type Key = (String, u64, String);

fn owner() -> UploadAccountOwner {
    UploadAccountOwner::new(
        CloudAccountKey::new("account-a").unwrap(),
        CloudAccountGeneration::new(1),
    )
}

fn endpoint() -> UploadEndpoint {
    UploadEndpoint::new(
        owner(),
        CloudApiBase::parse("https://cloud.example", false).unwrap(),
        CloudCredential::new("secret"),
    )
}

fn key(owner: &UploadAccountOwner, local: &LocalClipId) -> Key {
    (
        owner.account_key.as_str().to_owned(),
        owner.account_generation.get(),
        local.as_str().to_owned(),
    )
}

fn record(id: &str, status: &str) -> UploadRecord {
    let phase = match status {
        "uploaded_private" | "uploaded_public" => UploadPhase::Completed,
        "uploaded_processing" => UploadPhase::Abandoned,
        "processing" => UploadPhase::Processing,
        "failed" => UploadPhase::Failed,
        _ => panic!("unsupported fixture status"),
    };
    UploadRecord {
        token: DurableUploadToken {
            account_key: owner().account_key,
            account_generation: owner().account_generation,
            upload_generation: UploadGeneration::new(7),
            local_clip_id: LocalClipId::new(id).unwrap(),
            source_path: ClipPathIdentity::from_text(&format!(r"C:\Clips\{id}.mp4")).unwrap(),
        },
        client_clip_id: None,
        path: format!(r"C:\Clips\{id}.mp4"),
        visibility: "public".into(),
        phase,
        upload_status: status.into(),
        received_size_bytes: 100,
        file_size_bytes: 100,
        remote_clip_id: Some(format!("remote-{id}")),
        remote_url: Some(format!("https://clips.example/c/{id}")),
        error: None,
        local_deleted: false,
        updated_at_unix: 10,
    }
}

struct Records {
    owner: Mutex<UploadAccountOwner>,
    records: Mutex<HashMap<Key, UploadRecordCursor>>,
    active: Mutex<HashSet<DurableUploadToken>>,
    commits: AtomicUsize,
    removes: AtomicUsize,
}

impl Records {
    fn new(records: impl IntoIterator<Item = UploadRecord>) -> Self {
        let records = records
            .into_iter()
            .map(|record| {
                let key = key(&owner(), &record.token.local_clip_id);
                (
                    key,
                    UploadRecordCursor {
                        revision: 1,
                        record,
                    },
                )
            })
            .collect();
        Self {
            owner: Mutex::new(owner()),
            records: Mutex::new(records),
            active: Mutex::new(HashSet::new()),
            commits: AtomicUsize::new(0),
            removes: AtomicUsize::new(0),
        }
    }

    fn current(&self, local: &LocalClipId) -> Option<UploadRecordCursor> {
        self.records
            .lock()
            .unwrap()
            .get(&key(&owner(), local))
            .cloned()
    }

    fn replace(&self, mut cursor: UploadRecordCursor, message: &str) {
        cursor.revision += 1;
        cursor.record.error = Some(message.into());
        self.records
            .lock()
            .unwrap()
            .insert(key(&owner(), &cursor.record.token.local_clip_id), cursor);
    }
}

impl UploadStatusRecordPort for Records {
    fn status_cursor(
        &self,
        expected_owner: &UploadAccountOwner,
        local_clip_id: &LocalClipId,
    ) -> Result<Option<UploadRecordCursor>, UploadRecordError> {
        if &*self.owner.lock().unwrap() != expected_owner {
            return Err(UploadRecordError::new(
                UploadRecordErrorKind::AccountChanged,
                "account changed",
            ));
        }
        Ok(self.current(local_clip_id))
    }

    fn commit_status_sync(
        &self,
        expected: &UploadRecordCursor,
        replacement: UploadRecord,
    ) -> Result<UploadRecordCursor, UploadRecordError> {
        if *self.owner.lock().unwrap()
            != UploadAccountOwner::new(
                expected.record.token.account_key.clone(),
                expected.record.token.account_generation,
            )
        {
            return Err(UploadRecordError::new(
                UploadRecordErrorKind::AccountChanged,
                "account changed",
            ));
        }
        let mut records = self.records.lock().unwrap();
        let slot = key(&owner(), &expected.record.token.local_clip_id);
        if records.get(&slot) != Some(expected) {
            return Err(UploadRecordError::new(
                UploadRecordErrorKind::Superseded,
                "record changed",
            ));
        }
        let cursor = UploadRecordCursor {
            revision: expected.revision + 1,
            record: replacement,
        };
        records.insert(slot, cursor.clone());
        self.commits.fetch_add(1, Ordering::Relaxed);
        Ok(cursor)
    }

    fn remove_status_sync(&self, expected: &UploadRecordCursor) -> Result<(), UploadRecordError> {
        let mut records = self.records.lock().unwrap();
        let slot = key(&owner(), &expected.record.token.local_clip_id);
        if records.get(&slot) != Some(expected) {
            return Err(UploadRecordError::new(
                UploadRecordErrorKind::Superseded,
                "record changed",
            ));
        }
        records.remove(&slot);
        self.removes.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn is_active_token(&self, token: &DurableUploadToken) -> bool {
        self.active.lock().unwrap().contains(token)
    }
}

struct Remote {
    observation: Mutex<Result<RemoteClipObservation, UploadWorkError>>,
    calls: AtomicUsize,
    blocked: AtomicBool,
    release: tokio::sync::Notify,
}

impl Remote {
    fn new(observation: RemoteClipObservation) -> Self {
        Self {
            observation: Mutex::new(Ok(observation)),
            calls: AtomicUsize::new(0),
            blocked: AtomicBool::new(false),
            release: tokio::sync::Notify::new(),
        }
    }
}

impl UploadStatusRemotePort for Remote {
    fn inspect<'a>(
        &'a self,
        _endpoint: &'a UploadEndpoint,
        _remote_clip_id: &'a str,
        cancellation: &'a UploadCancellation,
    ) -> UploadFuture<'a, Result<RemoteClipObservation, UploadWorkError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::Release);
            if self.blocked.load(Ordering::Acquire) {
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => return Err(UploadWorkError::Canceled),
                    _ = self.release.notified() => {}
                }
            }
            self.observation.lock().unwrap().clone()
        })
    }
}

fn found(id: &str, status: &str, visibility: &str) -> RemoteClipObservation {
    RemoteClipObservation::Found(RemoteClipStatus {
        remote_clip_id: format!("remote-{id}"),
        visibility: visibility.into(),
        status: status.into(),
        public_url: Some(format!("https://clips.example/new/{id}")),
    })
}

#[test]
fn neutral_upload_status_service_is_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<UploadStatusSyncService>();
    let _remote = Arc::new(ReqwestUploadStatusRemote::new());
}

#[tokio::test]
async fn ready_pending_and_failed_observations_map_to_exact_durable_states() {
    for (remote_state, visibility, expected_phase, expected_status) in [
        (
            "ready",
            "private",
            UploadPhase::Completed,
            "uploaded_private",
        ),
        (
            "ready",
            "unlisted",
            UploadPhase::Completed,
            "uploaded_public",
        ),
        (
            "processing",
            "public",
            UploadPhase::Abandoned,
            "uploaded_processing",
        ),
        ("failed", "public", UploadPhase::Failed, "failed"),
    ] {
        let original = record("one", "uploaded_public");
        let records = Arc::new(Records::new([original.clone()]));
        let remote = Arc::new(Remote::new(found("one", remote_state, visibility)));
        let service = UploadStatusSyncService::with_ports(records.clone(), remote);
        let result = service
            .sync(
                &endpoint(),
                &original.token.local_clip_id,
                &UploadCancellation::default(),
            )
            .await
            .unwrap();
        let UploadStatusSyncOutcome::Updated(updated) = result else {
            panic!("expected an updated record");
        };
        assert_eq!(updated.phase, expected_phase);
        assert_eq!(updated.upload_status, expected_status);
        assert_eq!(updated.visibility, visibility);
        assert_eq!(updated.received_size_bytes, 100);
        if visibility == "private" {
            assert_eq!(updated.remote_url, None);
        }
        assert_eq!(records.commits.load(Ordering::Relaxed), 1);
    }
}

#[tokio::test]
async fn invalid_remote_identity_status_and_visibility_never_commit() {
    for observation in [
        RemoteClipObservation::Found(RemoteClipStatus {
            remote_clip_id: "other".into(),
            visibility: "public".into(),
            status: "ready".into(),
            public_url: None,
        }),
        found("one", "mystery", "public"),
        found("one", "ready", "friends"),
    ] {
        let original = record("one", "uploaded_public");
        let records = Arc::new(Records::new([original.clone()]));
        let service = UploadStatusSyncService::with_ports(
            records.clone(),
            Arc::new(Remote::new(observation)),
        );
        let error = service
            .sync(
                &endpoint(),
                &original.token.local_clip_id,
                &UploadCancellation::default(),
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind(), UploadStatusSyncErrorKind::InvalidResponse);
        assert_eq!(
            records
                .current(&original.token.local_clip_id)
                .unwrap()
                .record,
            original
        );
        assert_eq!(records.commits.load(Ordering::Relaxed), 0);
    }
}

#[tokio::test]
async fn missing_remote_requires_two_exact_observations_and_keeps_processing() {
    let finalized = record("final", "uploaded_public");
    let processing = record("processing", "uploaded_processing");
    let records = Arc::new(Records::new([finalized.clone(), processing.clone()]));
    let remote = Arc::new(Remote::new(RemoteClipObservation::Missing));
    let service = UploadStatusSyncService::with_ports(records.clone(), remote);
    let cancellation = UploadCancellation::default();

    let first = service
        .sync(&endpoint(), &finalized.token.local_clip_id, &cancellation)
        .await
        .unwrap();
    let UploadStatusSyncOutcome::Updated(marked) = first else {
        panic!("first missing observation must persist a marker");
    };
    assert_eq!(marked.error.as_deref(), Some(REMOTE_NOT_FOUND_SYNC_MARKER));
    let second = service
        .sync(&endpoint(), &finalized.token.local_clip_id, &cancellation)
        .await
        .unwrap();
    assert!(matches!(
        second,
        UploadStatusSyncOutcome::Removed { ref token, ref path }
            if token == &finalized.token && path == &finalized.path
    ));
    assert!(records.current(&finalized.token.local_clip_id).is_none());
    assert_eq!(records.removes.load(Ordering::Relaxed), 1);

    let kept = service
        .sync(&endpoint(), &processing.token.local_clip_id, &cancellation)
        .await
        .unwrap();
    assert_eq!(kept, UploadStatusSyncOutcome::Unchanged(processing));
}

#[tokio::test]
async fn exact_active_generation_is_not_remotely_reconciled() {
    let original = record("active", "processing");
    let records = Arc::new(Records::new([original.clone()]));
    records
        .active
        .lock()
        .unwrap()
        .insert(original.token.clone());
    let remote = Arc::new(Remote::new(found("active", "ready", "public")));
    let service = UploadStatusSyncService::with_ports(records, remote.clone());

    let result = service
        .sync(
            &endpoint(),
            &original.token.local_clip_id,
            &UploadCancellation::default(),
        )
        .await
        .unwrap();
    assert_eq!(result, UploadStatusSyncOutcome::Unchanged(original));
    assert_eq!(remote.calls.load(Ordering::Acquire), 0);
}

#[tokio::test]
async fn cancellation_and_delayed_record_replacement_fail_without_mutation() {
    let original = record("delayed", "uploaded_public");
    let records = Arc::new(Records::new([original.clone()]));
    let remote = Arc::new(Remote::new(found("delayed", "ready", "public")));
    remote.blocked.store(true, Ordering::Release);
    let service = UploadStatusSyncService::with_ports(records.clone(), remote.clone());
    let cancellation = UploadCancellation::default();
    let task = tokio::spawn({
        let service = service.clone();
        let local = original.token.local_clip_id.clone();
        let cancellation = cancellation.clone();
        async move { service.sync(&endpoint(), &local, &cancellation).await }
    });
    while remote.calls.load(Ordering::Acquire) == 0 {
        tokio::task::yield_now().await;
    }
    cancellation.cancel();
    let error = task.await.unwrap().unwrap_err();
    assert_eq!(error.kind(), UploadStatusSyncErrorKind::Canceled);
    assert_eq!(
        records
            .current(&original.token.local_clip_id)
            .unwrap()
            .record,
        original
    );

    let cancellation = UploadCancellation::default();
    let task = tokio::spawn({
        let service = service.clone();
        let local = original.token.local_clip_id.clone();
        async move { service.sync(&endpoint(), &local, &cancellation).await }
    });
    while remote.calls.load(Ordering::Acquire) < 2 {
        tokio::task::yield_now().await;
    }
    records.replace(
        records.current(&original.token.local_clip_id).unwrap(),
        "newer upload",
    );
    remote.release.notify_waiters();
    let error = task.await.unwrap().unwrap_err();
    assert_eq!(error.kind(), UploadStatusSyncErrorKind::Superseded);
    assert_eq!(
        records
            .current(&original.token.local_clip_id)
            .unwrap()
            .record
            .error
            .as_deref(),
        Some("newer upload")
    );
}

#[tokio::test]
async fn delayed_account_replacement_cannot_commit_the_old_accounts_result() {
    let original = record("account-race", "uploaded_public");
    let records = Arc::new(Records::new([original.clone()]));
    let remote = Arc::new(Remote::new(found("account-race", "ready", "public")));
    remote.blocked.store(true, Ordering::Release);
    let service = UploadStatusSyncService::with_ports(records.clone(), remote.clone());
    let task = tokio::spawn({
        let service = service.clone();
        let local = original.token.local_clip_id.clone();
        async move {
            service
                .sync(&endpoint(), &local, &UploadCancellation::default())
                .await
        }
    });
    while remote.calls.load(Ordering::Acquire) == 0 {
        tokio::task::yield_now().await;
    }
    *records.owner.lock().unwrap() = UploadAccountOwner::new(
        CloudAccountKey::new("account-b").unwrap(),
        CloudAccountGeneration::new(2),
    );
    remote.release.notify_waiters();

    let error = task.await.unwrap().unwrap_err();
    assert_eq!(error.kind(), UploadStatusSyncErrorKind::AccountChanged);
    assert_eq!(
        records
            .current(&original.token.local_clip_id)
            .unwrap()
            .record,
        original
    );
    assert_eq!(records.commits.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn duplicate_and_global_status_work_are_bounded_without_waiters() {
    let originals = [
        record("one", "uploaded_public"),
        record("two", "uploaded_public"),
        record("three", "uploaded_public"),
    ];
    let records = Arc::new(Records::new(originals.clone()));
    let remote = Arc::new(Remote::new(RemoteClipObservation::Missing));
    remote.blocked.store(true, Ordering::Release);
    let service = UploadStatusSyncService::with_ports(records, remote.clone());
    let mut tasks = Vec::new();
    for original in originals.iter().take(MAX_ACTIVE_UPLOAD_STATUS_SYNCS) {
        let service = service.clone();
        let local = original.token.local_clip_id.clone();
        tasks.push(tokio::spawn(async move {
            service
                .sync(&endpoint(), &local, &UploadCancellation::default())
                .await
        }));
    }
    while remote.calls.load(Ordering::Acquire) < MAX_ACTIVE_UPLOAD_STATUS_SYNCS {
        tokio::task::yield_now().await;
    }
    let duplicate = service
        .sync(
            &endpoint(),
            &originals[0].token.local_clip_id,
            &UploadCancellation::default(),
        )
        .await
        .unwrap_err();
    assert_eq!(duplicate.kind(), UploadStatusSyncErrorKind::Duplicate);
    let capacity = service
        .sync(
            &endpoint(),
            &originals[2].token.local_clip_id,
            &UploadCancellation::default(),
        )
        .await
        .unwrap_err();
    assert_eq!(capacity.kind(), UploadStatusSyncErrorKind::AtCapacity);
    remote.release.notify_waiters();
    for task in tasks {
        task.await.unwrap().unwrap();
    }
}
