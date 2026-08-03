//! Tauri adapters for the framework-neutral Clipline Cloud upload service.
//!
//! This module deliberately contains no HTTP protocol, multipart, filesystem
//! mutation, or process-global upload registry implementation. Those concerns
//! live in the permissively licensed `clipline-library` crate. The adapters
//! below bind that service to the durable app settings store and the bounded
//! desktop event channel.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use clipline_desktop::{
    CloudAccountOwner as DesktopCloudAccountOwner, CloudAccountScope, CloudUploadProgress,
    CloudUploadUpdateKind, Generation, UiEvent, UiEventSink,
};
use clipline_library::{
    account_key, ActiveFileRegistry, ClientClipId, ClipPathIdentity, CloudAccountFields,
    CloudAccountGeneration, DurableUploadToken, LocalClipId, LocalLibraryRepository,
    ReqwestUploadRemote, ReqwestUploadStatusRemote, ReqwestUploadTransport,
    StandardRepositoryFileSystem, StandardUploadPreparation, UploadAccountFence,
    UploadAccountOwner, UploadDeletePermit, UploadDeletionPort, UploadEventKind, UploadEventPort,
    UploadEventPortError, UploadGeneration, UploadPhase, UploadRecord, UploadRecordCursor,
    UploadRecordError, UploadRecordErrorKind, UploadRecordPort, UploadService, UploadServiceEvent,
    UploadStatusSyncService, UploadWorkError, MAX_ACTIVE_UPLOAD_JOBS,
};
use clipline_settings::{
    cloud_paths_equivalent, CloudAccountIdentity, CloudRecordCas, CloudRecordCasKind,
    CloudRecordSlot, CloudSettings, CloudUploadRecord, SettingsSnapshot,
    MAX_CLOUD_RECORD_CAS_SLOTS,
};
use tauri::{AppHandle, Manager, Wry};

use crate::app::RuntimeState;
use crate::desktop::tauri_sink::TauriUiEventSink;

/// Managed, window-independent upload state.
///
/// Dropping a WebView never drops this value. App shutdown seals the shared
/// service only after the shell's reversible quiescence has reached idle.
pub(crate) struct TauriUploadState {
    service: UploadService,
    status: UploadStatusSyncService,
}

impl std::fmt::Debug for TauriUploadState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TauriUploadState")
            .field("active_count", &self.service.active_count())
            .finish_non_exhaustive()
    }
}

impl TauriUploadState {
    /// Build the shipping upload service around the one process-wide active
    /// file registry shared by upload, Library mutations, and quota cleanup.
    pub(crate) fn build(
        app: AppHandle<Wry>,
        active_files: ActiveFileRegistry,
    ) -> Result<Self, String> {
        let sink = app
            .try_state::<TauriUiEventSink>()
            .ok_or_else(|| "Tauri UI event sink is not initialized".to_string())?
            .inner()
            .clone();
        let records = Arc::new(RuntimeUploadRecordPort::new(app.clone()));
        let transport = ReqwestUploadTransport::new().map_err(|error| error.to_string())?;
        let service = UploadService::new(
            active_files.clone(),
            Arc::new(RuntimeUploadAccountFence { app }),
            Arc::new(StandardUploadPreparation),
            Arc::new(transport),
            Arc::new(ReqwestUploadRemote::new()),
            Arc::new(RuntimeUploadDeletionPort { active_files }),
            records.clone(),
            Arc::new(TauriUploadEventPort::new(sink)),
        );
        let status = UploadStatusSyncService::new(
            service.clone(),
            Arc::new(ReqwestUploadStatusRemote::new()),
        );
        Ok(Self { service, status })
    }

    #[must_use]
    pub(crate) const fn service(&self) -> &UploadService {
        &self.service
    }

    #[must_use]
    pub(crate) const fn status(&self) -> &UploadStatusSyncService {
        &self.status
    }

    pub(crate) fn shutdown(&self) {
        self.service.shutdown();
    }
}

/// Delete through a repository rooted narrowly at the already-validated
/// source's parent. The source permit retains the exact validated path and file
/// identity, while deriving the root at deletion time avoids pinning uploads to
/// whichever media directory happened to be configured at app startup.
#[derive(Debug, Clone)]
pub(crate) struct RuntimeUploadDeletionPort {
    active_files: ActiveFileRegistry,
}

impl UploadDeletionPort for RuntimeUploadDeletionPort {
    fn delete_local(&self, permit: &UploadDeletePermit) -> Result<(), UploadWorkError> {
        let parent = permit.canonical_path().parent().ok_or_else(|| {
            UploadWorkError::failed("uploaded clip has no containing Library directory")
        })?;
        let repository = LocalLibraryRepository::with_seams(
            parent,
            Arc::new(StandardRepositoryFileSystem),
            Arc::new(self.active_files.clone()),
        )
        .map_err(|error| UploadWorkError::failed(error.to_string()))?;
        permit
            .delete_clip_and_sidecars_if_current(&repository)
            .map_err(|error| UploadWorkError::failed(error.to_string()))
    }
}

#[derive(Clone)]
pub(crate) struct RuntimeUploadAccountFence {
    app: AppHandle<Wry>,
}

impl UploadAccountFence for RuntimeUploadAccountFence {
    fn is_current(&self, owner: &UploadAccountOwner) -> bool {
        self.app
            .state::<RuntimeState>()
            .cloud_settings_snapshot()
            .ok()
            .and_then(|snapshot| upload_owner_from_snapshot(&snapshot).ok())
            .as_ref()
            == Some(owner)
    }
}

/// Durable settings adapter for the neutral service.
///
/// The trait is synchronous because admission and transition ordering is part
/// of its contract. Whole-settings writes can fsync; on a multi-thread Tokio
/// runtime `run_settings_io` marks those sections as blocking so another
/// worker can keep driving unrelated futures. Callers on a current-thread
/// runtime necessarily execute the synchronous contract inline.
pub(crate) struct RuntimeUploadRecordPort {
    app: AppHandle<Wry>,
    /// Settings intentionally persist only the shipping compatibility fields.
    /// Keep the exact live record so byte counters and `local_deleted` survive
    /// transitions and completion in this process. The entry is accepted only
    /// while its durable projection still equals the settings slot.
    live: Mutex<HashMap<DurableUploadToken, UploadRecord>>,
}

impl RuntimeUploadRecordPort {
    fn new(app: AppHandle<Wry>) -> Self {
        Self {
            app,
            live: Mutex::new(HashMap::with_capacity(MAX_ACTIVE_UPLOAD_JOBS)),
        }
    }

    fn snapshot(&self) -> Result<SettingsSnapshot, UploadRecordError> {
        run_settings_io(|| {
            self.app
                .state::<RuntimeState>()
                .cloud_settings_snapshot()
                .map_err(|error| record_error(UploadRecordErrorKind::Persistence, error))
        })
    }

    fn commit(&self, change: CloudRecordCas) -> Result<SettingsSnapshot, UploadRecordError> {
        run_settings_io(|| {
            self.app
                .state::<RuntimeState>()
                .compare_exchange_cloud_records(change)
                .map_err(classify_settings_error)
        })
    }

    fn remember(&self, record: UploadRecord) -> Result<(), UploadRecordError> {
        let mut live = self.live.lock().map_err(|_| {
            record_error(
                UploadRecordErrorKind::Persistence,
                "live upload record cache is unavailable",
            )
        })?;
        remember_live_record(&mut live, record);
        Ok(())
    }

    fn load_live_if_exact(
        &self,
        durable: &CloudUploadRecord,
        reloaded: &UploadRecord,
    ) -> Result<UploadRecord, UploadRecordError> {
        let mut live = self.live.lock().map_err(|_| {
            record_error(
                UploadRecordErrorKind::Persistence,
                "live upload record cache is unavailable",
            )
        })?;
        Ok(load_live_record(&mut live, durable, reloaded))
    }
}

fn remember_live_record(
    live: &mut HashMap<DurableUploadToken, UploadRecord>,
    record: UploadRecord,
) {
    let token = record.token.clone();
    live.retain(|existing, _| {
        existing.account_key != token.account_key
            || existing.account_generation != token.account_generation
            || existing.local_clip_id != token.local_clip_id
    });
    // The cache is an optimization, never the durability authority. If an old
    // completed job remains after its command waiter has gone, evict that stale
    // terminal entry first. Never evict the token being remembered: its command
    // completion still needs exact local-deletion and byte fields. A
    // nonterminal fallback eviction remains safe because the running job owns
    // its cursor and settings own its durable state.
    if live.len() >= MAX_ACTIVE_UPLOAD_JOBS {
        let evicted_terminal = live.iter().find_map(|(existing, cached)| {
            (existing != &token && cached.phase.is_terminal()).then(|| existing.clone())
        });
        if let Some(evicted) = evicted_terminal {
            live.remove(&evicted);
        }
    }
    if live.len() >= MAX_ACTIVE_UPLOAD_JOBS {
        if let Some(evicted) = live.keys().find(|existing| *existing != &token).cloned() {
            live.remove(&evicted);
        }
    }
    live.insert(token, record);
}

fn load_live_record(
    live: &mut HashMap<DurableUploadToken, UploadRecord>,
    durable: &CloudUploadRecord,
    reloaded: &UploadRecord,
) -> UploadRecord {
    match live.get(&reloaded.token) {
        Some(cached) if cloud_record_from_upload(cached) == *durable => {
            // Loads are deliberately non-consuming: a concurrent status cursor
            // must not steal exact volatile fields from job completion.
            cached.clone()
        }
        Some(_) => {
            live.remove(&reloaded.token);
            reloaded.clone()
        }
        None => reloaded.clone(),
    }
}

impl UploadRecordPort for RuntimeUploadRecordPort {
    fn allocate_generation(
        &self,
        owner: &UploadAccountOwner,
        local_clip_id: &LocalClipId,
        source: &clipline_library::ValidatedClipPath,
    ) -> Result<UploadGeneration, UploadRecordError> {
        let snapshot = self.snapshot()?;
        ensure_snapshot_owner(&snapshot, owner)?;
        next_generation(
            &snapshot.document.cloud,
            local_clip_id,
            source.display_path(),
            source.comparison_identity(),
        )
    }

    fn load(
        &self,
        owner: &UploadAccountOwner,
        local_clip_id: &LocalClipId,
    ) -> Result<Option<UploadRecordCursor>, UploadRecordError> {
        let snapshot = self.snapshot()?;
        ensure_snapshot_owner(&snapshot, owner)?;
        let Some(durable) = snapshot.document.cloud.uploads.get(local_clip_id.as_str()) else {
            return Ok(None);
        };
        let reloaded = reload_record(owner, durable)?;
        let record = self.load_live_if_exact(durable, &reloaded)?;
        Ok(Some(UploadRecordCursor {
            revision: snapshot.revision.get(),
            record,
        }))
    }

    fn admit(&self, record: UploadRecord) -> Result<UploadRecordCursor, UploadRecordError> {
        let snapshot = self.snapshot()?;
        let owner = owner_from_record(&record);
        ensure_snapshot_owner(&snapshot, &owner)?;
        let change = admission_cas(&snapshot, &record)?;
        let after = self.commit(change)?;
        ensure_snapshot_owner(&after, &owner)?;
        self.remember(record.clone())?;
        Ok(UploadRecordCursor {
            revision: after.revision.get(),
            record,
        })
    }

    fn compare_exchange(
        &self,
        expected: &UploadRecordCursor,
        replacement: UploadRecord,
    ) -> Result<UploadRecordCursor, UploadRecordError> {
        if expected.record.token != replacement.token {
            return Err(record_error(
                UploadRecordErrorKind::Superseded,
                "upload record replacement changed its durable owner",
            ));
        }
        let snapshot = self.snapshot()?;
        let owner = owner_from_record(&expected.record);
        ensure_snapshot_owner(&snapshot, &owner)?;
        let change = transition_cas(&snapshot, &expected.record, &replacement)?;
        let after = self.commit(change)?;
        ensure_snapshot_owner(&after, &owner)?;
        self.remember(replacement.clone())?;
        Ok(UploadRecordCursor {
            revision: after.revision.get(),
            record: replacement,
        })
    }

    fn remove_exact(&self, expected: &UploadRecordCursor) -> Result<(), UploadRecordError> {
        let snapshot = self.snapshot()?;
        let owner = owner_from_record(&expected.record);
        ensure_snapshot_owner(&snapshot, &owner)?;
        let change = removal_cas(&snapshot, &expected.record)?;
        let after = self.commit(change)?;
        ensure_snapshot_owner(&after, &owner)?;
        // The settings commit is the authority. Clear the optional volatile
        // overlay only after that exact CAS succeeds; a poisoned optimization
        // cache must not turn a committed removal into a reported failure.
        if let Ok(mut live) = self.live.lock() {
            live.remove(&expected.record.token);
        }
        Ok(())
    }
}

#[derive(Clone)]
pub(crate) struct TauriUploadEventPort {
    sink: TauriUiEventSink,
}

impl TauriUploadEventPort {
    fn new(sink: TauriUiEventSink) -> Self {
        Self { sink }
    }
}

impl UploadEventPort for TauriUploadEventPort {
    fn try_publish(&self, event: UploadServiceEvent) -> Result<(), UploadEventPortError> {
        let event = desktop_upload_event(event)?;
        self.sink
            .try_publish(event)
            .map(|_| ())
            .map_err(|error| UploadEventPortError(error.to_string()))
    }
}

fn desktop_upload_event(event: UploadServiceEvent) -> Result<UiEvent, UploadEventPortError> {
    if event.kind == UploadEventKind::Bytes && event.notice.is_some() {
        return Err(UploadEventPortError(
            "byte-only upload progress cannot carry a notice".into(),
        ));
    }
    let account = DesktopCloudAccountOwner::new(
        event.record.token.account_key.as_str(),
        CloudAccountScope::new(event.record.token.account_generation.get()),
    )
    .map_err(|error| UploadEventPortError(error.to_string()))?;
    let update = match event.kind {
        UploadEventKind::Bytes => CloudUploadUpdateKind::Bytes,
        UploadEventKind::State => CloudUploadUpdateKind::State,
    };
    let notice = event.notice.map(|notice| notice.message);
    let terminal = event.record.phase.is_terminal();
    Ok(UiEvent::CloudUploadProgress {
        account,
        generation: Generation::new(event.record.token.upload_generation.get()),
        update,
        progress: CloudUploadProgress {
            local_clip_id: event.record.token.local_clip_id.as_str().to_owned(),
            path: event.record.path,
            upload_status: event.record.upload_status,
            terminal,
            received_size_bytes: event.record.received_size_bytes,
            file_size_bytes: event.record.file_size_bytes,
            remote_clip_id: event.record.remote_clip_id,
            remote_url: event.record.remote_url,
            error: event.record.error,
        },
        notice,
    })
}

fn run_settings_io<T>(operation: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current().map(|handle| handle.runtime_flavor()) {
        Ok(tokio::runtime::RuntimeFlavor::MultiThread) => tokio::task::block_in_place(operation),
        _ => operation(),
    }
}

fn upload_owner_from_snapshot(
    snapshot: &SettingsSnapshot,
) -> Result<UploadAccountOwner, UploadRecordError> {
    if !snapshot.document.cloud.connected() {
        return Err(record_error(
            UploadRecordErrorKind::AccountChanged,
            "Clipline Cloud is not connected",
        ));
    }
    let key = account_key(&CloudAccountFields {
        host_url: snapshot.document.cloud.host_url.clone(),
        connected_user_id: snapshot
            .document
            .cloud
            .connected_user_id
            .clone()
            .unwrap_or_default(),
        credential_target: snapshot
            .document
            .cloud
            .credential_target
            .clone()
            .unwrap_or_default(),
    })
    .map_err(|error| record_error(UploadRecordErrorKind::Persistence, error.to_string()))?;
    Ok(UploadAccountOwner::new(
        key,
        CloudAccountGeneration::new(snapshot.account_generation.get()),
    ))
}

fn ensure_snapshot_owner(
    snapshot: &SettingsSnapshot,
    expected: &UploadAccountOwner,
) -> Result<(), UploadRecordError> {
    if upload_owner_from_snapshot(snapshot)? == *expected {
        Ok(())
    } else {
        Err(record_error(
            UploadRecordErrorKind::AccountChanged,
            "cloud account changed while upload state was in flight",
        ))
    }
}

fn owner_from_record(record: &UploadRecord) -> UploadAccountOwner {
    UploadAccountOwner::new(
        record.token.account_key.clone(),
        record.token.account_generation,
    )
}

fn next_generation(
    cloud: &CloudSettings,
    local_clip_id: &LocalClipId,
    source_display_path: &str,
    source_identity: &ClipPathIdentity,
) -> Result<UploadGeneration, UploadRecordError> {
    let maximum = cloud
        .uploads
        .iter()
        .filter(|(key, record)| {
            key.as_str() == local_clip_id.as_str()
                || record.local_clip_id == local_clip_id.as_str()
                || cloud_paths_equivalent(&record.path, source_display_path)
                || ClipPathIdentity::from_text(&record.path).as_ref() == Some(source_identity)
        })
        .filter_map(|(_, record)| record.upload_generation)
        .max()
        .unwrap_or(0);
    maximum
        .checked_add(1)
        .map(UploadGeneration::new)
        .ok_or_else(|| {
            record_error(
                UploadRecordErrorKind::Persistence,
                "durable upload generation is exhausted",
            )
        })
}

fn admission_cas(
    snapshot: &SettingsSnapshot,
    record: &UploadRecord,
) -> Result<CloudRecordCas, UploadRecordError> {
    let replacement = cloud_record_from_upload(record);
    let stable_key = record.token.local_clip_id.as_str();
    let expected = reconciled_slots(
        &snapshot.document.cloud,
        stable_key,
        &record.path,
        None,
        true,
    )?;
    let durable_maximum = expected
        .iter()
        .filter_map(|slot| slot.record.as_ref()?.upload_generation)
        .max()
        .unwrap_or(0);
    if record.token.upload_generation.get() <= durable_maximum {
        return Err(record_error(
            UploadRecordErrorKind::Contended,
            "a newer durable upload generation appeared before admission",
        ));
    }
    Ok(CloudRecordCas {
        account: CloudAccountIdentity::from_settings(&snapshot.document.cloud),
        account_generation: snapshot.account_generation,
        kind: CloudRecordCasKind::Admit {
            upload_generation: record.token.upload_generation.get(),
        },
        expected,
        replacement: Some(CloudRecordSlot {
            key: stable_key.to_owned(),
            record: Some(replacement),
        }),
    })
}

fn transition_cas(
    snapshot: &SettingsSnapshot,
    expected_record: &UploadRecord,
    replacement: &UploadRecord,
) -> Result<CloudRecordCas, UploadRecordError> {
    let stable_key = expected_record.token.local_clip_id.as_str();
    let expected_durable = cloud_record_from_upload(expected_record);
    let current = snapshot.document.cloud.uploads.get(stable_key);
    if current != Some(&expected_durable) {
        return Err(record_error(
            UploadRecordErrorKind::Superseded,
            "durable upload record changed before compare-and-swap",
        ));
    }
    let expected = reconciled_slots(
        &snapshot.document.cloud,
        stable_key,
        &replacement.path,
        Some(&expected_record.path),
        false,
    )?;
    let kind = if expected_record.token.upload_generation == UploadGeneration::INITIAL
        || expected_record.phase.is_terminal()
    {
        CloudRecordCasKind::StatusSync
    } else {
        CloudRecordCasKind::Advance {
            upload_generation: replacement.token.upload_generation.get(),
        }
    };
    Ok(CloudRecordCas {
        account: CloudAccountIdentity::from_settings(&snapshot.document.cloud),
        account_generation: snapshot.account_generation,
        kind,
        expected,
        replacement: Some(CloudRecordSlot {
            key: stable_key.to_owned(),
            record: Some(cloud_record_from_upload(replacement)),
        }),
    })
}

fn removal_cas(
    snapshot: &SettingsSnapshot,
    expected_record: &UploadRecord,
) -> Result<CloudRecordCas, UploadRecordError> {
    let stable_key = expected_record.token.local_clip_id.as_str();
    let expected_durable = cloud_record_from_upload(expected_record);
    if snapshot.document.cloud.uploads.get(stable_key) != Some(&expected_durable) {
        return Err(record_error(
            UploadRecordErrorKind::Superseded,
            "durable upload record changed before exact removal",
        ));
    }
    let expected = reconciled_slots(
        &snapshot.document.cloud,
        stable_key,
        &expected_record.path,
        Some(&expected_record.path),
        false,
    )?;
    Ok(CloudRecordCas {
        account: CloudAccountIdentity::from_settings(&snapshot.document.cloud),
        account_generation: snapshot.account_generation,
        kind: CloudRecordCasKind::StatusSync,
        expected,
        replacement: None,
    })
}

fn reconciled_slots(
    cloud: &CloudSettings,
    stable_key: &str,
    replacement_path: &str,
    prior_path: Option<&str>,
    include_absent_stable: bool,
) -> Result<Vec<CloudRecordSlot>, UploadRecordError> {
    let mut expected = Vec::new();
    for (key, record) in &cloud.uploads {
        let path_alias = cloud_paths_equivalent(&record.path, replacement_path)
            || prior_path.is_some_and(|prior| cloud_paths_equivalent(&record.path, prior));
        if key == stable_key || path_alias {
            expected.push(CloudRecordSlot {
                key: key.clone(),
                record: Some(record.clone()),
            });
        }
    }
    if !expected.iter().any(|slot| slot.key == stable_key) {
        if include_absent_stable {
            expected.push(CloudRecordSlot {
                key: stable_key.to_owned(),
                record: None,
            });
        } else {
            return Err(record_error(
                UploadRecordErrorKind::Superseded,
                "durable upload record is missing",
            ));
        }
    }
    if expected.len() > MAX_CLOUD_RECORD_CAS_SLOTS {
        return Err(record_error(
            UploadRecordErrorKind::Persistence,
            format!(
                "upload path has {} durable aliases; maximum is {MAX_CLOUD_RECORD_CAS_SLOTS}",
                expected.len()
            ),
        ));
    }
    expected.sort_by(|left, right| left.key.cmp(&right.key));
    Ok(expected)
}

/// Compatibility projection returned by the Tauri command and persisted by
/// the exact-CAS adapter. Volatile byte counters and local-deletion state stay
/// on the neutral `UploadRecord`/command envelope rather than being invented
/// in the legacy settings schema.
pub(crate) fn cloud_record_from_upload(record: &UploadRecord) -> CloudUploadRecord {
    let mut durable = CloudUploadRecord {
        local_clip_id: record.token.local_clip_id.as_str().to_owned(),
        client_clip_id: record
            .client_clip_id
            .as_ref()
            .map(|id| id.as_str().to_owned()),
        // Generation zero is an in-memory sentinel for records written before
        // durable upload ownership existed. Project it back to `None` so a
        // status sync cannot silently claim legacy ownership.
        upload_generation: (record.token.upload_generation != UploadGeneration::INITIAL)
            .then(|| record.token.upload_generation.get()),
        path: record.path.clone(),
        remote_clip_id: record.remote_clip_id.clone(),
        remote_url: record.remote_url.clone(),
        visibility: record.visibility.clone(),
        upload_status: record.upload_status.clone(),
        error: record.error.clone(),
        updated_at_unix: record.updated_at_unix,
    };
    durable.normalize();
    durable
}

fn reload_record(
    owner: &UploadAccountOwner,
    durable: &CloudUploadRecord,
) -> Result<UploadRecord, UploadRecordError> {
    let local_clip_id = LocalClipId::new(durable.local_clip_id.clone())
        .map_err(|error| record_error(UploadRecordErrorKind::Persistence, error.to_string()))?;
    let client_clip_id = durable
        .client_clip_id
        .clone()
        .map(ClientClipId::new)
        .transpose()
        .map_err(|error| record_error(UploadRecordErrorKind::Persistence, error.to_string()))?;
    let source_path = ClipPathIdentity::from_text(&durable.path).ok_or_else(|| {
        record_error(
            UploadRecordErrorKind::Persistence,
            "durable upload record has an invalid source path",
        )
    })?;
    let phase = phase_for_persisted_status(&durable.upload_status)?;
    Ok(UploadRecord {
        token: DurableUploadToken {
            account_key: owner.account_key.clone(),
            account_generation: owner.account_generation,
            upload_generation: UploadGeneration::new(durable.upload_generation.unwrap_or(0)),
            local_clip_id,
            source_path,
        },
        client_clip_id,
        path: durable.path.clone(),
        visibility: durable.visibility.clone(),
        phase,
        upload_status: durable.upload_status.clone(),
        received_size_bytes: 0,
        file_size_bytes: 0,
        remote_clip_id: durable.remote_clip_id.clone(),
        remote_url: durable.remote_url.clone(),
        error: durable.error.clone(),
        local_deleted: false,
        updated_at_unix: durable.updated_at_unix,
    })
}

/// Explicit restart mapping for the compatibility settings schema. Phases
/// that share one durable status intentionally collapse to the safest restart
/// state; a detached job is never inferred to still be running after restart.
fn phase_for_persisted_status(status: &str) -> Result<UploadPhase, UploadRecordError> {
    match status {
        "queued" | "retrying" => Ok(UploadPhase::Queued),
        "uploading" => Ok(UploadPhase::Uploading),
        "processing" => Ok(UploadPhase::Processing),
        "uploaded_processing" => Ok(UploadPhase::Abandoned),
        "uploaded_private" | "uploaded_public" => Ok(UploadPhase::Completed),
        "failed" => Ok(UploadPhase::Failed),
        "canceled" => Ok(UploadPhase::Canceled),
        other => Err(record_error(
            UploadRecordErrorKind::Persistence,
            format!("durable upload status {other:?} cannot be reloaded"),
        )),
    }
}

fn classify_settings_error(error: String) -> UploadRecordError {
    let lower = error.to_ascii_lowercase();
    let kind = if lower.contains("account changed") || lower.contains("cloud account generation") {
        UploadRecordErrorKind::AccountChanged
    } else if lower.contains("stale cloud record") {
        UploadRecordErrorKind::Superseded
    } else if lower.contains("stale settings revision") || lower.contains("contended") {
        UploadRecordErrorKind::Contended
    } else {
        UploadRecordErrorKind::Persistence
    };
    record_error(kind, error)
}

fn record_error(kind: UploadRecordErrorKind, message: impl Into<String>) -> UploadRecordError {
    UploadRecordError::new(kind, message)
}

#[cfg(test)]
mod tests {
    use clipline_library::{
        clip_sidecar_paths, local_clip_id_for_source, CloudAccountKey, UploadNotice,
        UploadServiceEvent,
    };
    use clipline_settings::{AccountGeneration, AppSettings, CloudRecordCasKind, SettingsRevision};
    use clipline_test_utils::TestDir;

    use super::*;

    fn connected_snapshot() -> SettingsSnapshot {
        let mut document = AppSettings::default();
        document.cloud.host_url = "https://cloud.example".into();
        document.cloud.connected_user_id = Some("user-1".into());
        document.cloud.credential_target = Some("credential-1".into());
        SettingsSnapshot {
            account: CloudAccountIdentity::from_settings(&document.cloud),
            document,
            revision: SettingsRevision::INITIAL,
            account_generation: AccountGeneration::INITIAL,
        }
    }

    fn owner(snapshot: &SettingsSnapshot) -> UploadAccountOwner {
        upload_owner_from_snapshot(snapshot).unwrap()
    }

    fn record(snapshot: &SettingsSnapshot, generation: u64, path: &str) -> UploadRecord {
        let owner = owner(snapshot);
        UploadRecord {
            token: DurableUploadToken {
                account_key: owner.account_key,
                account_generation: owner.account_generation,
                upload_generation: UploadGeneration::new(generation),
                local_clip_id: LocalClipId::new("local-1").unwrap(),
                source_path: ClipPathIdentity::from_text(path).unwrap(),
            },
            client_clip_id: None,
            path: path.into(),
            visibility: "private".into(),
            phase: UploadPhase::Queued,
            upload_status: "queued".into(),
            received_size_bytes: 0,
            file_size_bytes: 0,
            remote_clip_id: None,
            remote_url: None,
            error: None,
            local_deleted: false,
            updated_at_unix: 10,
        }
    }

    #[test]
    fn legacy_status_reload_mapping_is_explicit_and_fail_closed() {
        for (status, phase) in [
            ("queued", UploadPhase::Queued),
            ("retrying", UploadPhase::Queued),
            ("uploading", UploadPhase::Uploading),
            ("processing", UploadPhase::Processing),
            ("uploaded_processing", UploadPhase::Abandoned),
            ("uploaded_private", UploadPhase::Completed),
            ("uploaded_public", UploadPhase::Completed),
            ("failed", UploadPhase::Failed),
            ("canceled", UploadPhase::Canceled),
        ] {
            assert_eq!(phase_for_persisted_status(status).unwrap(), phase);
        }
        assert_eq!(
            phase_for_persisted_status("not_uploaded")
                .unwrap_err()
                .kind(),
            UploadRecordErrorKind::Persistence
        );
    }

    #[test]
    fn admission_reconciles_windows_aliases_in_one_exact_cas() {
        let mut snapshot = connected_snapshot();
        let mut legacy = cloud_record_from_upload(&record(&snapshot, 7, r"\\?\C:\Media\Clip.mp4"));
        legacy.local_clip_id = "legacy-client-id".into();
        snapshot
            .document
            .cloud
            .uploads
            .insert("legacy-client-id".into(), legacy.clone());
        snapshot.document.cloud.uploads.insert(
            "unrelated".into(),
            CloudUploadRecord {
                local_clip_id: "unrelated".into(),
                path: r"C:\Media\other.mp4".into(),
                ..legacy.clone()
            },
        );
        let admitted = record(&snapshot, 8, r"c:\media\CLIP.mp4");

        let cas = admission_cas(&snapshot, &admitted).unwrap();

        assert_eq!(
            cas.kind,
            CloudRecordCasKind::Admit {
                upload_generation: 8
            }
        );
        assert_eq!(cas.expected.len(), 2);
        assert!(cas.expected.iter().any(|slot| {
            slot.key == "legacy-client-id" && slot.record.as_ref() == Some(&legacy)
        }));
        assert!(cas
            .expected
            .iter()
            .any(|slot| slot.key == "local-1" && slot.record.is_none()));
        assert!(!cas.expected.iter().any(|slot| slot.key == "unrelated"));
        assert_eq!(cas.replacement.unwrap().key, "local-1");
    }

    #[test]
    fn generation_allocation_includes_path_equivalent_legacy_aliases() {
        let mut snapshot = connected_snapshot();
        let mut legacy = cloud_record_from_upload(&record(&snapshot, 41, r"C:\Media\Clip.mp4"));
        legacy.local_clip_id = "old-client-id".into();
        snapshot
            .document
            .cloud
            .uploads
            .insert("old-client-id".into(), legacy);

        let next = next_generation(
            &snapshot.document.cloud,
            &LocalClipId::new("local-1").unwrap(),
            r"\\?\c:\MEDIA\clip.mp4",
            &ClipPathIdentity::from_text(r"c:\media\clip.mp4").unwrap(),
        )
        .unwrap();

        assert_eq!(next, UploadGeneration::new(42));
    }

    #[test]
    fn admission_rechecks_alias_generation_after_allocation_race() {
        let mut snapshot = connected_snapshot();
        let mut raced = cloud_record_from_upload(&record(&snapshot, 42, r"C:\Media\Clip.mp4"));
        raced.local_clip_id = "racing-alias".into();
        snapshot
            .document
            .cloud
            .uploads
            .insert("racing-alias".into(), raced);

        let stale_candidate = record(&snapshot, 42, r"c:\media\clip.mp4");
        let error = admission_cas(&snapshot, &stale_candidate).unwrap_err();

        assert_eq!(error.kind(), UploadRecordErrorKind::Contended);
    }

    #[test]
    fn live_projection_round_trips_durable_fields_and_reload_defaults_volatile_fields() {
        let snapshot = connected_snapshot();
        let mut live = record(&snapshot, 3, r"C:\Media\clip.mp4");
        live.client_clip_id = Some(ClientClipId::new("client-3").unwrap());
        live.phase = UploadPhase::Completed;
        live.upload_status = "uploaded_public".into();
        live.visibility = "public".into();
        live.received_size_bytes = 999;
        live.file_size_bytes = 999;
        live.remote_clip_id = Some("remote-3".into());
        live.remote_url = Some("https://cloud.example/clip/remote-3".into());
        live.local_deleted = true;
        let durable = cloud_record_from_upload(&live);

        let reloaded = reload_record(&owner(&snapshot), &durable).unwrap();

        assert_eq!(cloud_record_from_upload(&reloaded), durable);
        assert_eq!(reloaded.phase, UploadPhase::Completed);
        assert_eq!(reloaded.received_size_bytes, 0);
        assert_eq!(reloaded.file_size_bytes, 0);
        assert!(!reloaded.local_deleted);
    }

    #[test]
    fn legacy_status_sync_preserves_absent_generation() {
        let mut snapshot = connected_snapshot();
        let mut legacy = cloud_record_from_upload(&record(&snapshot, 1, r"C:\Media\clip.mp4"));
        legacy.upload_generation = None;
        legacy.upload_status = "uploaded_processing".into();
        snapshot
            .document
            .cloud
            .uploads
            .insert("local-1".into(), legacy.clone());
        let expected = reload_record(&owner(&snapshot), &legacy).unwrap();
        assert_eq!(expected.token.upload_generation, UploadGeneration::INITIAL);
        let mut replacement = expected.clone();
        replacement.phase = UploadPhase::Completed;
        replacement.upload_status = "uploaded_private".into();
        replacement.remote_clip_id = Some("remote-legacy".into());

        let cas = transition_cas(&snapshot, &expected, &replacement).unwrap();

        assert_eq!(cas.kind, CloudRecordCasKind::StatusSync);
        assert_eq!(
            cas.replacement.unwrap().record.unwrap().upload_generation,
            None
        );
    }

    #[test]
    fn exact_removal_includes_path_aliases_and_no_replacement() {
        let mut snapshot = connected_snapshot();
        let expected = record(&snapshot, 5, r"C:\Media\clip.mp4");
        let durable = cloud_record_from_upload(&expected);
        snapshot
            .document
            .cloud
            .uploads
            .insert("local-1".into(), durable.clone());
        let mut alias = durable.clone();
        alias.local_clip_id = "legacy-alias".into();
        alias.path = r"\\?\c:\MEDIA\CLIP.mp4".into();
        snapshot
            .document
            .cloud
            .uploads
            .insert("legacy-alias".into(), alias.clone());

        let cas = removal_cas(&snapshot, &expected).unwrap();

        assert_eq!(cas.kind, CloudRecordCasKind::StatusSync);
        assert!(cas.replacement.is_none());
        assert_eq!(cas.expected.len(), 2);
        assert!(cas
            .expected
            .iter()
            .any(|slot| slot.key == "local-1" && slot.record.as_ref() == Some(&durable)));
        assert!(cas
            .expected
            .iter()
            .any(|slot| slot.key == "legacy-alias" && slot.record.as_ref() == Some(&alias)));
    }

    #[test]
    fn exact_removal_preserves_legacy_none_generation_shape() {
        let mut snapshot = connected_snapshot();
        let mut durable = cloud_record_from_upload(&record(&snapshot, 1, r"C:\Media\legacy.mp4"));
        durable.upload_generation = None;
        durable.upload_status = "uploaded_processing".into();
        snapshot
            .document
            .cloud
            .uploads
            .insert("local-1".into(), durable.clone());
        let expected = reload_record(&owner(&snapshot), &durable).unwrap();

        let cas = removal_cas(&snapshot, &expected).unwrap();

        assert_eq!(cas.expected.len(), 1);
        assert_eq!(cas.expected[0].record, Some(durable));
        assert!(cas.replacement.is_none());
    }

    #[test]
    fn exact_removal_rejects_stale_expected_record_shape() {
        let mut snapshot = connected_snapshot();
        let stale = record(&snapshot, 3, r"C:\Media\clip.mp4");
        let current = record(&snapshot, 4, r"C:\Media\clip.mp4");
        snapshot
            .document
            .cloud
            .uploads
            .insert("local-1".into(), cloud_record_from_upload(&current));

        let error = removal_cas(&snapshot, &stale).unwrap_err();

        assert_eq!(error.kind(), UploadRecordErrorKind::Superseded);
    }

    #[test]
    fn deletion_port_derives_each_sources_current_parent_root() {
        let directory = TestDir::new("clipline-cloud-upload", "dynamic-delete-root");
        let active_files = ActiveFileRegistry::new();
        let port = RuntimeUploadDeletionPort {
            active_files: active_files.clone(),
        };

        for (generation, folder) in [(1, "first"), (2, "second")] {
            let root = directory.path().join(folder);
            std::fs::create_dir_all(&root).unwrap();
            let path = root.join("clip.mp4");
            std::fs::write(&path, b"clip").unwrap();
            let sidecars = clip_sidecar_paths(&path).into_array();
            for sidecar in &sidecars {
                std::fs::write(sidecar, b"sidecar").unwrap();
            }
            let repository = LocalLibraryRepository::with_seams(
                &root,
                Arc::new(StandardRepositoryFileSystem),
                Arc::new(active_files.clone()),
            )
            .unwrap();
            let source = repository
                .validate_clip_path(path.to_string_lossy().as_ref())
                .unwrap();
            let token = DurableUploadToken {
                account_key: CloudAccountKey::new("account-1").unwrap(),
                account_generation: CloudAccountGeneration::new(1),
                upload_generation: UploadGeneration::new(generation),
                local_clip_id: local_clip_id_for_source(source.file_identity()),
                source_path: source.comparison_identity().clone(),
            };
            let permit = active_files
                .acquire_upload(&source, token)
                .unwrap()
                .into_delete_permit()
                .unwrap();

            port.delete_local(&permit).unwrap();

            assert!(!path.exists());
            assert!(sidecars.iter().all(|sidecar| !sidecar.exists()));
        }
    }

    #[test]
    fn event_mapping_preserves_exact_account_generation_kind_and_notice() {
        let snapshot = connected_snapshot();
        let mut upload = record(&snapshot, 9, r"C:\Media\clip.mp4");
        upload.phase = UploadPhase::Failed;
        upload.upload_status = "failed".into();
        upload.error = Some("network failed".into());
        let mapped = desktop_upload_event(UploadServiceEvent {
            kind: UploadEventKind::State,
            record: upload,
            notice: Some(UploadNotice {
                id: "durable-notice".into(),
                message: "Upload failed".into(),
            }),
        })
        .unwrap();

        let UiEvent::CloudUploadProgress {
            account,
            generation,
            update,
            progress,
            notice,
        } = mapped
        else {
            panic!("wrong event kind");
        };
        assert_eq!(account.account_key(), owner(&snapshot).account_key.as_str());
        assert_eq!(account.account_generation(), CloudAccountScope::new(1));
        assert_eq!(generation, Generation::new(9));
        assert_eq!(update, CloudUploadUpdateKind::State);
        assert_eq!(progress.local_clip_id, "local-1");
        assert!(progress.terminal);
        assert_eq!(progress.error.as_deref(), Some("network failed"));
        assert_eq!(notice.as_deref(), Some("Upload failed"));
    }

    #[test]
    fn byte_event_with_notice_is_rejected_before_queue_publication() {
        let snapshot = connected_snapshot();
        let error = desktop_upload_event(UploadServiceEvent {
            kind: UploadEventKind::Bytes,
            record: record(&snapshot, 1, r"C:\Media\clip.mp4"),
            notice: Some(UploadNotice {
                id: "invalid".into(),
                message: "invalid".into(),
            }),
        })
        .unwrap_err();
        assert!(error.to_string().contains("byte-only"));
    }

    #[test]
    fn settings_error_classification_is_fail_closed() {
        assert_eq!(
            classify_settings_error("cloud account changed".into()).kind(),
            UploadRecordErrorKind::AccountChanged
        );
        assert_eq!(
            classify_settings_error("stale cloud record".into()).kind(),
            UploadRecordErrorKind::Superseded
        );
        assert_eq!(
            classify_settings_error("stale settings revision".into()).kind(),
            UploadRecordErrorKind::Contended
        );
        assert_eq!(
            classify_settings_error("disk write failed".into()).kind(),
            UploadRecordErrorKind::Persistence
        );
    }
}
