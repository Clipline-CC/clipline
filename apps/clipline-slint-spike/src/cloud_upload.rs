//! Bounded non-blocking fanout from the process-owned upload service into the
//! native catalog and desktop contracts.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clipline_desktop::{
    CloudAccountOwner, CloudAccountScope, CloudUploadProgress, CloudUploadUpdateKind, Generation,
    UiEvent, UiEventSendError, UiEventSender,
};
use clipline_library::{ActiveFileRegistry, CatalogUploadOptions, CatalogUploadVisibility};
use clipline_library::{
    CatalogResult, CatalogResultSender, CatalogUploadProjection, DurableUploadToken,
    ExpectedResultOwner, LocalLibraryRepository, ReqwestUploadRemote, ReqwestUploadStatusRemote,
    ReqwestUploadTransport, ResolvedLocalClip, ResultPortError, SettingsUploadAccountFence,
    SettingsUploadRecordPort, StandardRepositoryFileSystem, StandardUploadPreparation,
    UploadAccountOwner, UploadCancellation, UploadDeletePermit, UploadDeletionPort, UploadEndpoint,
    UploadEventKind, UploadEventPort, UploadEventPortError, UploadIntent, UploadPhase,
    UploadService, UploadServiceEvent, UploadStartRequest, UploadStatusSyncOutcome,
    UploadStatusSyncService, UploadSummary, UploadWorkError, MAX_ACTIVE_UPLOAD_JOBS,
    MAX_UPLOAD_SUMMARIES,
};
use clipline_settings::SettingsStore;

/// Worst-case union while both downstream queues are stalled: 16 immediately
/// visible durable rows, 16 disjoint restart status candidates, and all 16
/// upload-service jobs. A terminal transition replaces its active-job slot, so
/// it never needs a fourth category or a best-effort retry outside this owner.
pub const MAX_NATIVE_UPLOAD_FANOUT_SLOTS: usize = MAX_UPLOAD_SUMMARIES * 2 + MAX_ACTIVE_UPLOAD_JOBS;
pub const MAX_NATIVE_UPLOAD_FANOUT_BYTES: usize = 32 * 1024 * 1024;
const MAX_FANOUT_ATTEMPTS_PER_PUMP: usize = 32;
const UPLOAD_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const _: () = assert!(MAX_ACTIVE_UPLOAD_JOBS == MAX_UPLOAD_SUMMARIES);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UploadFanoutPumpReport {
    pub delivered: usize,
    pub retained: usize,
    pub discarded: usize,
}

#[derive(Debug, Clone)]
struct UploadSlot {
    token: DurableUploadToken,
    catalog: Option<CatalogResult>,
    desktop: Option<UiEvent>,
}

impl UploadSlot {
    fn estimated_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            .saturating_add(
                self.catalog
                    .as_ref()
                    .map_or(0, CatalogResult::estimated_byte_size),
            )
            .saturating_add(self.desktop.as_ref().map_or(0, estimated_desktop_bytes))
    }
}

#[derive(Default)]
struct FanoutState {
    slots: Vec<UploadSlot>,
}

/// Upload state is durably committed before this port is called. The fanout
/// therefore retains one latest bounded payload per exact job until each
/// downstream queue accepts it. Byte updates coalesce, while a pending state
/// barrier remains a state barrier when later byte progress arrives.
#[derive(Clone)]
pub struct NativeUploadEventFanout {
    catalog: CatalogResultSender,
    desktop: UiEventSender,
    state: Arc<Mutex<FanoutState>>,
}

/// Process-owned native upload services. It borrows the Cloud Tokio runtime
/// only while starting work or waiting for shutdown, so window destruction
/// cannot detach jobs and runtime teardown remains explicitly ordered.
pub struct NativeUploadRuntime {
    service: UploadService,
    status: UploadStatusSyncService,
    repository: Arc<LocalLibraryRepository>,
    store: SettingsStore,
    status_cancel: UploadCancellation,
    status_tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

#[derive(Debug, Default)]
pub struct NativeUploadHydration {
    pub visible: Vec<clipline_library::UploadRecord>,
    pub status_candidates: Vec<clipline_library::UploadRecord>,
}

impl NativeUploadRuntime {
    pub fn open(
        store: SettingsStore,
        media_root: &Path,
        active_files: ActiveFileRegistry,
        events: Arc<dyn UploadEventPort>,
    ) -> Result<Self, String> {
        let repository = Arc::new(
            LocalLibraryRepository::with_seams(
                media_root,
                Arc::new(StandardRepositoryFileSystem),
                Arc::new(active_files.clone()),
            )
            .map_err(|error| error.to_string())?,
        );
        let records = Arc::new(SettingsUploadRecordPort::new(store.clone()));
        let transport = ReqwestUploadTransport::new().map_err(|error| error.to_string())?;
        let service = UploadService::new(
            active_files,
            Arc::new(SettingsUploadAccountFence::new(store.clone())),
            Arc::new(StandardUploadPreparation),
            Arc::new(transport),
            Arc::new(ReqwestUploadRemote::new()),
            Arc::new(NativeUploadDeletionPort {
                repository: Arc::clone(&repository),
            }),
            records,
            events,
        );
        let status = UploadStatusSyncService::new(
            service.clone(),
            Arc::new(ReqwestUploadStatusRemote::new()),
        );
        Ok(Self {
            service,
            status,
            repository,
            store,
            status_cancel: UploadCancellation::default(),
            status_tasks: Mutex::new(Vec::with_capacity(1)),
        })
    }

    pub fn start(
        &self,
        handle: &tokio::runtime::Handle,
        endpoint: UploadEndpoint,
        target: &ResolvedLocalClip,
        options: CatalogUploadOptions,
    ) -> Result<DurableUploadToken, String> {
        let source = self
            .repository
            .validate_clip_path(&target.path)
            .map_err(|error| error.to_string())?;
        if source.comparison_identity() != &target.identity {
            return Err("upload target identity changed before admission".into());
        }
        if target
            .expected_file_identity
            .is_some_and(|expected| expected != source.file_identity())
        {
            return Err("upload target file was replaced before admission".into());
        }
        let intent = UploadIntent {
            title: options.title,
            description: options.description,
            visibility: match options.visibility {
                CatalogUploadVisibility::Private => "private",
                CatalogUploadVisibility::Public => "public",
                CatalogUploadVisibility::Unlisted => "unlisted",
            }
            .into(),
            audio_track_ids: (!options.audio_track_ids.is_empty())
                .then_some(options.audio_track_ids),
            delete_local_after_upload: options.delete_local_after_upload,
        };
        // UploadService::start deliberately requires an entered runtime but
        // the returned handle is detached-safe. The process-owned runtime
        // remains alive until `shutdown` below reaches idle.
        let _entered = handle.enter();
        let job = self
            .service
            .start(UploadStartRequest {
                endpoint,
                source,
                intent,
            })
            .map_err(|error| error.to_string())?;
        Ok(job.token().clone())
    }

    #[must_use]
    pub fn cancel(&self, token: &DurableUploadToken) -> bool {
        self.service.cancel(token)
    }

    #[must_use]
    pub const fn status(&self) -> &UploadStatusSyncService {
        &self.status
    }

    /// Reload only records that have meaningful durable presentation after a
    /// process restart. Orphaned queued/uploading phases have no active job and
    /// are intentionally not exposed as cancelable work.
    pub fn hydrate(&self, owner: &UploadAccountOwner) -> Result<NativeUploadHydration, String> {
        let snapshot = self.store.snapshot().map_err(|error| error.to_string())?;
        let current = clipline_library::upload_account_owner_from_snapshot(&snapshot)
            .map_err(|error| error.to_string())?;
        if &current != owner {
            return Err("native upload hydration belongs to a replaced Cloud account".into());
        }
        let mut durable = snapshot
            .document
            .cloud
            .uploads
            .values()
            .cloned()
            .collect::<Vec<_>>();
        durable.sort_by(|left, right| {
            right
                .updated_at_unix
                .cmp(&left.updated_at_unix)
                .then_with(|| left.local_clip_id.cmp(&right.local_clip_id))
        });
        let mut hydration = NativeUploadHydration {
            visible: Vec::with_capacity(MAX_UPLOAD_SUMMARIES),
            status_candidates: Vec::with_capacity(MAX_UPLOAD_SUMMARIES),
        };
        for durable in durable {
            if hydration.visible.len() == MAX_UPLOAD_SUMMARIES
                && hydration.status_candidates.len() == MAX_UPLOAD_SUMMARIES
            {
                break;
            }
            let Ok(record) = clipline_library::reload_upload_record(owner, &durable) else {
                continue;
            };
            let remote_status_candidate = record.remote_clip_id.is_some()
                && matches!(
                    record.phase,
                    UploadPhase::Processing | UploadPhase::Abandoned | UploadPhase::Completed
                );
            if remote_status_candidate && hydration.status_candidates.len() < MAX_UPLOAD_SUMMARIES {
                hydration.status_candidates.push(record.clone());
            }
            if matches!(
                record.phase,
                UploadPhase::Completed
                    | UploadPhase::Canceled
                    | UploadPhase::Failed
                    | UploadPhase::Abandoned
            ) && hydration.visible.len() < MAX_UPLOAD_SUMMARIES
            {
                hydration.visible.push(record);
            }
        }
        Ok(hydration)
    }

    /// Refresh restart-surviving remote records serially. The status service
    /// itself remains capped at two, while one serial bootstrap task avoids a
    /// startup burst turning legitimate records into capacity failures.
    pub fn refresh_hydrated_statuses(
        &self,
        handle: &tokio::runtime::Handle,
        endpoint: UploadEndpoint,
        records: &[clipline_library::UploadRecord],
        fanout: NativeUploadEventFanout,
    ) -> Result<(), String> {
        let ids = records
            .iter()
            .filter(|record| record.remote_clip_id.is_some())
            .map(|record| record.token.local_clip_id.clone())
            .collect::<Vec<_>>();
        if ids.is_empty() {
            return Ok(());
        }
        let status = self.status.clone();
        let cancellation = self.status_cancel.clone();
        let task = handle.spawn(async move {
            for id in ids {
                if cancellation.is_canceled() {
                    break;
                }
                match status.sync(&endpoint, &id, &cancellation).await {
                    Ok(
                        UploadStatusSyncOutcome::Unchanged(record)
                        | UploadStatusSyncOutcome::Updated(record),
                    ) => {
                        let _ = fanout.try_publish(UploadServiceEvent {
                            kind: UploadEventKind::State,
                            record,
                            notice: None,
                        });
                    }
                    Ok(UploadStatusSyncOutcome::Removed { token, .. }) => {
                        let _ = fanout.enqueue_removed(token);
                    }
                    Ok(UploadStatusSyncOutcome::MissingRecord) | Err(_) => {}
                }
            }
        });
        let mut tasks = self
            .status_tasks
            .lock()
            .map_err(|_| "native upload status task registry is unavailable".to_owned())?;
        tasks.retain(|task| !task.is_finished());
        tasks.push(task);
        Ok(())
    }

    pub fn shutdown(&self, handle: &tokio::runtime::Handle) -> Result<(), String> {
        self.status_cancel.cancel();
        let tasks = self
            .status_tasks
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .drain(..)
            .collect::<Vec<_>>();
        let status_result = handle
            .block_on(tokio::time::timeout(UPLOAD_SHUTDOWN_TIMEOUT, async {
                for task in tasks {
                    let _ = task.await;
                }
            }))
            .map_err(|_| "native Cloud status work did not stop within 5 seconds".to_owned());
        self.service.shutdown();
        let upload_result = handle
            .block_on(tokio::time::timeout(
                UPLOAD_SHUTDOWN_TIMEOUT,
                self.service.wait_idle(),
            ))
            .map_err(|_| "native Cloud uploads did not stop within 5 seconds".to_owned());
        status_result.and(upload_result)
    }
}

struct NativeUploadDeletionPort {
    repository: Arc<LocalLibraryRepository>,
}

impl UploadDeletionPort for NativeUploadDeletionPort {
    fn delete_local(&self, permit: &UploadDeletePermit) -> Result<(), UploadWorkError> {
        permit
            .delete_clip_and_sidecars_if_current(&self.repository)
            .map_err(|error| UploadWorkError::failed(error.to_string()))
    }
}

impl NativeUploadEventFanout {
    #[must_use]
    pub fn new(catalog: CatalogResultSender, desktop: UiEventSender) -> Self {
        Self {
            catalog,
            desktop,
            state: Arc::new(Mutex::new(FanoutState {
                slots: Vec::with_capacity(MAX_NATIVE_UPLOAD_FANOUT_SLOTS),
            })),
        }
    }

    #[must_use]
    pub fn pending_slots(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .slots
            .len()
    }

    pub fn enqueue_removed(&self, token: DurableUploadToken) -> Result<(), UploadEventPortError> {
        let catalog = CatalogResult::UploadRemoved {
            token: token.clone(),
        };
        catalog
            .validate_bounds()
            .map_err(|error| UploadEventPortError(error.to_string()))?;
        let desktop = desktop_removal(&token)?;
        self.stage(token, catalog, desktop)
    }

    /// Makes one bounded, non-blocking pass. Full downstream queues retain the
    /// exact latest payload for a later event-loop tick. Stale-account and
    /// disconnected outcomes are terminal for that downstream copy.
    pub fn pump(&self) -> UploadFanoutPumpReport {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let mut report = UploadFanoutPumpReport::default();
        let mut attempts = 0;
        for slot in &mut state.slots {
            if attempts >= MAX_FANOUT_ATTEMPTS_PER_PUMP {
                break;
            }
            if let Some(result) = slot.catalog.take() {
                attempts += 1;
                let expected = ExpectedResultOwner::Upload(slot.token.clone());
                match self.catalog.try_send_recoverable(result, expected) {
                    Ok(_) => report.delivered += 1,
                    Err(rejected) if retry_catalog_error(rejected.error) => {
                        slot.catalog = Some(rejected.result);
                        report.retained += 1;
                    }
                    Err(_) => report.discarded += 1,
                }
            }
            if attempts >= MAX_FANOUT_ATTEMPTS_PER_PUMP {
                break;
            }
            if let Some(event) = slot.desktop.take() {
                attempts += 1;
                let retained = event.clone();
                match self.desktop.try_publish(event) {
                    Ok(_) => report.delivered += 1,
                    Err(UiEventSendError::Full { .. }) => {
                        slot.desktop = Some(retained);
                        report.retained += 1;
                    }
                    Err(_) => report.discarded += 1,
                }
            }
        }
        state
            .slots
            .retain(|slot| slot.catalog.is_some() || slot.desktop.is_some());
        report
    }

    fn stage(
        &self,
        token: DurableUploadToken,
        catalog: CatalogResult,
        desktop: UiEvent,
    ) -> Result<(), UploadEventPortError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| UploadEventPortError("native upload fanout is unavailable".into()))?;

        if let Some(existing) = state.slots.iter().position(|slot| slot.token == token) {
            let old_slot = state.slots[existing].clone();
            let candidate = UploadSlot {
                token,
                catalog: Some(match old_slot.catalog.clone() {
                    Some(current) => merge_catalog(current, catalog),
                    None => catalog,
                }),
                desktop: Some(match old_slot.desktop.clone() {
                    Some(current) => merge_desktop(current, desktop),
                    None => desktop,
                }),
            };
            let projected = estimated_state_bytes(&state)
                .saturating_sub(old_slot.estimated_bytes())
                .saturating_add(candidate.estimated_bytes());
            if projected > MAX_NATIVE_UPLOAD_FANOUT_BYTES {
                return Err(UploadEventPortError(format!(
                    "native upload fanout exceeds {MAX_NATIVE_UPLOAD_FANOUT_BYTES} bytes"
                )));
            }
            state.slots[existing] = candidate;
            return Ok(());
        }

        let same_owner = |slot: &UploadSlot| {
            slot.token.account_key == token.account_key
                && slot.token.account_generation == token.account_generation
                && slot.token.local_clip_id == token.local_clip_id
        };
        if state
            .slots
            .iter()
            .any(|slot| same_owner(slot) && slot.token.upload_generation > token.upload_generation)
        {
            return Ok(());
        }
        state.slots.retain(|slot| !same_owner(slot));
        if state.slots.len() >= MAX_NATIVE_UPLOAD_FANOUT_SLOTS {
            return Err(UploadEventPortError(format!(
                "native upload fanout is full at {MAX_NATIVE_UPLOAD_FANOUT_SLOTS} slots"
            )));
        }
        let slot = UploadSlot {
            token,
            catalog: Some(catalog),
            desktop: Some(desktop),
        };
        let projected = estimated_state_bytes(&state).saturating_add(slot.estimated_bytes());
        if projected > MAX_NATIVE_UPLOAD_FANOUT_BYTES {
            return Err(UploadEventPortError(format!(
                "native upload fanout exceeds {MAX_NATIVE_UPLOAD_FANOUT_BYTES} bytes"
            )));
        }
        state.slots.push(slot);
        Ok(())
    }
}

impl UploadEventPort for NativeUploadEventFanout {
    fn try_publish(&self, event: UploadServiceEvent) -> Result<(), UploadEventPortError> {
        if event.kind == UploadEventKind::Bytes && event.notice.is_some() {
            return Err(UploadEventPortError(
                "byte-only upload progress cannot carry a notice".into(),
            ));
        }
        let token = event.record.token.clone();
        let projection = CatalogUploadProjection::new(token.clone(), upload_summary(&event.record))
            .map_err(|error| UploadEventPortError(error.to_string()))?;
        let catalog = match (event.kind, event.record.phase.is_terminal()) {
            (UploadEventKind::Bytes, _) | (UploadEventKind::State, false) => {
                CatalogResult::UploadByteProgress {
                    token: token.clone(),
                    progress: projection.summary,
                }
            }
            (UploadEventKind::State, true) => CatalogResult::UploadCompleted {
                token: token.clone(),
                result: projection.summary,
            },
        };
        let desktop = desktop_upload_event(&event)?;
        self.stage(token, catalog, desktop)
    }
}

fn upload_summary(record: &clipline_library::UploadRecord) -> UploadSummary {
    UploadSummary {
        local_clip_id: record.token.local_clip_id.as_str().to_owned(),
        path: record.path.clone(),
        upload_status: record.upload_status.clone(),
        received_size_bytes: record.received_size_bytes,
        file_size_bytes: record.file_size_bytes,
        remote_clip_id: record.remote_clip_id.clone(),
        // Durable compatibility storage permits 64 KiB URLs/errors, while a
        // native catalog row deliberately owns at most 16 KiB per field. An
        // oversized URL is not actionable in the native surface; retain it
        // durably but omit it from presentation. Error text is safely clipped
        // on a UTF-8 boundary for display.
        remote_url: record
            .remote_url
            .as_ref()
            .filter(|url| url.len() <= clipline_library::MAX_CATALOG_STRING_BYTES)
            .cloned(),
        error: record
            .error
            .as_ref()
            .map(|error| bounded_upload_text(error, clipline_library::MAX_CATALOG_STRING_BYTES)),
    }
}

fn desktop_upload_event(event: &UploadServiceEvent) -> Result<UiEvent, UploadEventPortError> {
    let summary = upload_summary(&event.record);
    let account = CloudAccountOwner::new(
        event.record.token.account_key.as_str(),
        CloudAccountScope::new(event.record.token.account_generation.get()),
    )
    .map_err(|error| UploadEventPortError(error.to_string()))?;
    Ok(UiEvent::CloudUploadProgress {
        account,
        generation: Generation::new(event.record.token.upload_generation.get()),
        update: match event.kind {
            UploadEventKind::Bytes => CloudUploadUpdateKind::Bytes,
            UploadEventKind::State => CloudUploadUpdateKind::State,
        },
        progress: CloudUploadProgress {
            local_clip_id: summary.local_clip_id,
            path: summary.path,
            upload_status: summary.upload_status,
            terminal: event.record.phase.is_terminal(),
            received_size_bytes: summary.received_size_bytes,
            file_size_bytes: summary.file_size_bytes,
            remote_clip_id: summary.remote_clip_id,
            remote_url: summary.remote_url,
            error: summary.error,
        },
        notice: event.notice.as_ref().map(|notice| notice.message.clone()),
    })
}

fn bounded_upload_text(value: &str, maximum: usize) -> String {
    if value.len() <= maximum {
        return value.to_owned();
    }
    let mut end = maximum;
    while end != 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

fn desktop_removal(token: &DurableUploadToken) -> Result<UiEvent, UploadEventPortError> {
    let account = CloudAccountOwner::new(
        token.account_key.as_str(),
        CloudAccountScope::new(token.account_generation.get()),
    )
    .map_err(|error| UploadEventPortError(error.to_string()))?;
    Ok(UiEvent::CloudUploadRemoved {
        account,
        generation: Generation::new(token.upload_generation.get()),
        local_clip_id: token.local_clip_id.as_str().to_owned(),
    })
}

fn merge_catalog(current: CatalogResult, incoming: CatalogResult) -> CatalogResult {
    match (current, incoming) {
        (
            CatalogResult::UploadCompleted { token, .. },
            CatalogResult::UploadByteProgress { progress, .. },
        ) => CatalogResult::UploadCompleted {
            token,
            result: progress,
        },
        (_, incoming) => incoming,
    }
}

fn merge_desktop(current: UiEvent, incoming: UiEvent) -> UiEvent {
    match (current, incoming) {
        (
            UiEvent::CloudUploadProgress {
                account,
                generation,
                update: CloudUploadUpdateKind::State,
                notice,
                ..
            },
            UiEvent::CloudUploadProgress { progress, .. },
        ) => UiEvent::CloudUploadProgress {
            account,
            generation,
            update: CloudUploadUpdateKind::State,
            progress,
            notice,
        },
        (_, incoming) => incoming,
    }
}

fn retry_catalog_error(error: ResultPortError) -> bool {
    matches!(
        error,
        ResultPortError::Full { .. } | ResultPortError::ByteCapacity { .. }
    )
}

fn estimated_state_bytes(state: &FanoutState) -> usize {
    state
        .slots
        .iter()
        .map(UploadSlot::estimated_bytes)
        .fold(0_usize, usize::saturating_add)
}

fn estimated_desktop_bytes(event: &UiEvent) -> usize {
    match event {
        UiEvent::CloudUploadProgress {
            account,
            progress,
            notice,
            ..
        } => account
            .account_key()
            .len()
            .saturating_add(progress.local_clip_id.len())
            .saturating_add(progress.path.len())
            .saturating_add(progress.upload_status.len())
            .saturating_add(progress.remote_clip_id.as_ref().map_or(0, String::len))
            .saturating_add(progress.remote_url.as_ref().map_or(0, String::len))
            .saturating_add(progress.error.as_ref().map_or(0, String::len))
            .saturating_add(notice.as_ref().map_or(0, String::len)),
        UiEvent::CloudUploadRemoved {
            account,
            local_clip_id,
            ..
        } => account
            .account_key()
            .len()
            .saturating_add(local_clip_id.len()),
        _ => 0,
    }
}
