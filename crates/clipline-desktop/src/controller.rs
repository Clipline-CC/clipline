use thiserror::Error;

use crate::{
    CatalogSummarySnapshot, CloudAccountOwner, CloudAccountScope, CloudUploadSnapshot,
    CloudUploadUpdateKind, DesktopSnapshot, GameSnapshot, Generation, GenerationError,
    MediaRootSnapshot, MicrophonePhase, MicrophoneSnapshot, Notice, NoticeKind, RecorderEvent,
    RecorderSnapshot, RecorderStatus, Revision, SavedReplay, StorageStatus, UiAction, UiEffect,
    UiEvent, WindowLifecycleSnapshot,
};

pub const MAX_PENDING_NOTICES: usize = 64;
pub const MAX_NOTICE_MESSAGE_BYTES: usize = 64 * 1024;
pub const MAX_ACTIVE_UPLOADS: usize = 16;
pub const DESKTOP_SNAPSHOT_SCHEMA_VERSION: u32 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyEventOutcome {
    Applied { revision: Revision },
    Unchanged,
    Stale,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DispatchOutcome {
    pub effect: UiEffect,
    pub changed: bool,
    pub revision: Revision,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ControllerError {
    #[error("desktop snapshot generation is exhausted")]
    GenerationExhausted,
    #[error("pending notice capacity {capacity} is exhausted")]
    NoticesFull { capacity: usize },
    #[error("active upload capacity {capacity} is exhausted")]
    UploadsFull { capacity: usize },
    #[error("notice contains {actual} bytes; maximum is {maximum}")]
    NoticeTooLarge { actual: usize, maximum: usize },
    #[error("notice message must not be empty")]
    NoticeEmpty,
    #[error("invalid cloud progress: {0}")]
    InvalidCloudProgress(&'static str),
    #[error("invalid recorder metric")]
    InvalidRecorderMetric,
    #[error("invalid desktop snapshot: {0}")]
    InvalidSnapshot(&'static str),
}

impl From<GenerationError> for ControllerError {
    fn from(_: GenerationError) -> Self {
        Self::GenerationExhausted
    }
}

pub struct DesktopController<S> {
    snapshot: DesktopSnapshot<S>,
}

impl<S> DesktopController<S>
where
    S: Clone + PartialEq,
{
    pub fn new(settings: S, startup_warnings: Vec<String>) -> Result<Self, ControllerError> {
        for message in &startup_warnings {
            validate_notice_message(message)?;
        }
        if startup_warnings.len() > MAX_PENDING_NOTICES {
            return Err(ControllerError::NoticesFull {
                capacity: MAX_PENDING_NOTICES,
            });
        }
        let notices = startup_warnings
            .into_iter()
            .enumerate()
            .map(|(index, message)| Notice {
                id: u64::try_from(index).unwrap_or(u64::MAX).saturating_add(1),
                kind: NoticeKind::StartupWarning,
                message,
                created_revision: Revision::INITIAL,
                account: None,
            })
            .collect::<Vec<_>>();
        let notice_sequence = u64::try_from(notices.len()).unwrap_or(u64::MAX);
        Ok(Self {
            snapshot: DesktopSnapshot {
                schema_version: DESKTOP_SNAPSHOT_SCHEMA_VERSION,
                revision: Revision::INITIAL,
                settings_revision: Revision::INITIAL,
                settings,
                lifecycle: WindowLifecycleSnapshot::default(),
                recorder: RecorderSnapshot::default(),
                storage: None,
                media_root: None,
                latest_saved: None,
                game: GameSnapshot::default(),
                microphone: MicrophoneSnapshot::default(),
                cloud_account_generation: CloudAccountScope::INITIAL,
                current_cloud_account: None,
                uploads: Vec::new(),
                library_revision: Revision::INITIAL,
                catalog: CatalogSummarySnapshot::default(),
                enrichment_generation: Generation::INITIAL,
                notices,
                notice_sequence,
            },
        })
    }

    pub fn from_snapshot(snapshot: DesktopSnapshot<S>) -> Result<Self, ControllerError> {
        validate_snapshot(&snapshot)?;
        Ok(Self { snapshot })
    }

    #[must_use]
    pub fn snapshot(&self) -> DesktopSnapshot<S> {
        self.snapshot.clone()
    }

    /// Read the process-owned Library invalidation cursor without cloning the
    /// bounded desktop snapshot. Native shells poll this scalar to coalesce
    /// refresh work while leaving upload strings and notices in place.
    #[must_use]
    pub const fn library_revision(&self) -> Revision {
        self.snapshot.library_revision
    }

    pub fn dispatch(&mut self, action: UiAction) -> Result<DispatchOutcome, ControllerError> {
        let effect = action.effect();
        let mut changed = false;
        if let UiAction::AcknowledgeNotice { notice_id } = action {
            if self.snapshot.notices.first().map(|notice| notice.id) == Some(notice_id) {
                let revision = self.snapshot.revision.checked_next()?;
                self.snapshot.notices.remove(0);
                self.snapshot.revision = revision;
                changed = true;
            }
        }
        Ok(DispatchOutcome {
            effect,
            changed,
            revision: self.snapshot.revision,
        })
    }

    pub fn apply_event(&mut self, event: UiEvent) -> Result<ApplyEventOutcome, ControllerError> {
        let mut next = self.snapshot.clone();
        let changed = match event {
            UiEvent::WindowLifecycle { snapshot } => {
                if snapshot.revision < next.lifecycle.revision
                    || (snapshot.revision == next.lifecycle.revision && snapshot != next.lifecycle)
                {
                    return Ok(ApplyEventOutcome::Stale);
                }
                if snapshot == next.lifecycle {
                    false
                } else {
                    next.lifecycle = snapshot;
                    true
                }
            }
            UiEvent::Recorder { generation, event } => {
                if generation < next.recorder.generation {
                    return Ok(ApplyEventOutcome::Stale);
                }
                let mut changed = false;
                if generation > next.recorder.generation {
                    next.recorder.generation = generation;
                    next.recorder.status = RecorderStatus::default();
                    changed = true;
                }
                changed | apply_recorder_event(&mut next, event)?
            }
            UiEvent::MicMonitor {
                generation,
                monitor,
            } => {
                if microphone_event_is_stale(&next.microphone, generation) {
                    return Ok(ApplyEventOutcome::Stale);
                }
                let replacement = MicrophoneSnapshot {
                    generation,
                    phase: MicrophonePhase::Monitoring,
                    monitor: Some(monitor),
                    error: None,
                };
                if replacement == next.microphone {
                    false
                } else {
                    next.microphone = replacement;
                    true
                }
            }
            UiEvent::MicTestError {
                generation,
                message,
            } => {
                if microphone_event_is_stale(&next.microphone, generation) {
                    return Ok(ApplyEventOutcome::Stale);
                }
                let replacement = MicrophoneSnapshot {
                    generation,
                    phase: MicrophonePhase::Failed,
                    monitor: None,
                    error: Some(message),
                };
                if replacement == next.microphone {
                    false
                } else {
                    next.microphone = replacement;
                    true
                }
            }
            UiEvent::MicTestStopped { generation } => {
                if microphone_event_is_stale(&next.microphone, generation) {
                    return Ok(ApplyEventOutcome::Stale);
                }
                let replacement = MicrophoneSnapshot {
                    generation,
                    phase: MicrophonePhase::Stopped,
                    monitor: None,
                    error: None,
                };
                if replacement == next.microphone {
                    false
                } else {
                    next.microphone = replacement;
                    true
                }
            }
            UiEvent::GameDetection {
                generation,
                detection,
            } => {
                if generation < next.game.generation {
                    return Ok(ApplyEventOutcome::Stale);
                }
                let replacement = GameSnapshot {
                    generation,
                    detection: Some(detection),
                };
                if replacement == next.game {
                    false
                } else {
                    next.game = replacement;
                    true
                }
            }
            UiEvent::CloudAccountChanged {
                generation,
                account,
            } => {
                if generation < next.cloud_account_generation
                    || (generation == next.cloud_account_generation
                        && next.current_cloud_account != account)
                {
                    return Ok(ApplyEventOutcome::Stale);
                }
                if account
                    .as_ref()
                    .is_some_and(|owner| owner.account_generation() != generation)
                {
                    return Err(ControllerError::InvalidCloudProgress(
                        "cloud account owner generation does not match its event",
                    ));
                }
                if next.current_cloud_account == account
                    && next.cloud_account_generation == generation
                {
                    false
                } else {
                    next.cloud_account_generation = generation;
                    next.current_cloud_account.clone_from(&account);
                    next.uploads
                        .retain(|upload| account.as_ref() == Some(&upload.account));
                    next.notices.retain(|notice| {
                        notice.account.is_none() || notice.account.as_ref() == account.as_ref()
                    });
                    next.library_revision = next.library_revision.checked_next()?;
                    true
                }
            }
            UiEvent::CloudUploadProgress {
                account,
                generation,
                update,
                progress,
                notice,
            } => {
                if next.current_cloud_account.as_ref() != Some(&account) {
                    return Ok(ApplyEventOutcome::Stale);
                }
                if update == CloudUploadUpdateKind::Bytes && notice.is_some() {
                    return Err(ControllerError::InvalidCloudProgress(
                        "byte-only progress cannot carry a notice",
                    ));
                }
                match apply_cloud_progress(
                    &mut next,
                    account.clone(),
                    generation,
                    update,
                    progress,
                )? {
                    CloudProgressOutcome::Stale => return Ok(ApplyEventOutcome::Stale),
                    CloudProgressOutcome::Unchanged => false,
                    CloudProgressOutcome::BytesChanged => true,
                    CloudProgressOutcome::StateChanged => {
                        next.library_revision = next.library_revision.checked_next()?;
                        if let Some(message) = notice {
                            push_notice(
                                &mut next,
                                NoticeKind::CloudUpload,
                                message,
                                Some(account),
                            )?;
                        }
                        true
                    }
                }
            }
            UiEvent::CloudUploadRemoved {
                account,
                generation,
                local_clip_id,
            } => {
                if next.current_cloud_account.as_ref() != Some(&account) {
                    return Ok(ApplyEventOutcome::Stale);
                }
                let Ok(index) = next.uploads.binary_search_by(|upload| {
                    upload_order(upload, &account, local_clip_id.as_str())
                }) else {
                    return Ok(ApplyEventOutcome::Unchanged);
                };
                if next.uploads[index].generation != generation {
                    return Ok(ApplyEventOutcome::Stale);
                }
                next.uploads.remove(index);
                next.library_revision = next.library_revision.checked_next()?;
                true
            }
            UiEvent::EnrichmentUpdated { generation } => {
                if generation < next.enrichment_generation {
                    return Ok(ApplyEventOutcome::Stale);
                }
                if generation == next.enrichment_generation {
                    false
                } else {
                    next.enrichment_generation = generation;
                    next.library_revision = next.library_revision.checked_next()?;
                    true
                }
            }
            UiEvent::CatalogSummaryChanged { summary } => {
                if summary.revision < next.catalog.revision
                    || (summary.revision == next.catalog.revision && summary != next.catalog)
                {
                    return Ok(ApplyEventOutcome::Stale);
                }
                if summary == next.catalog {
                    false
                } else {
                    next.catalog = summary;
                    true
                }
            }
            UiEvent::UserError { message } => {
                push_notice(&mut next, NoticeKind::Error, message, None)?;
                true
            }
        };
        if !changed {
            return Ok(ApplyEventOutcome::Unchanged);
        }
        next.revision = next.revision.checked_next()?;
        let revision = next.revision;
        self.snapshot = next;
        Ok(ApplyEventOutcome::Applied { revision })
    }

    pub fn replace_settings(&mut self, settings: S) -> Result<bool, ControllerError> {
        if self.snapshot.settings == settings {
            return Ok(false);
        }
        let settings_revision = self.snapshot.settings_revision.checked_next()?;
        let revision = self.snapshot.revision.checked_next()?;
        self.snapshot.settings = settings;
        self.snapshot.settings_revision = settings_revision;
        self.snapshot.revision = revision;
        Ok(true)
    }

    pub fn set_recorder_desired(&mut self, desired: bool) -> Result<bool, ControllerError> {
        if self.snapshot.recorder.desired == desired {
            return Ok(false);
        }
        let revision = self.snapshot.revision.checked_next()?;
        self.snapshot.recorder.desired = desired;
        self.snapshot.revision = revision;
        Ok(true)
    }
}

fn apply_recorder_event<S>(
    snapshot: &mut DesktopSnapshot<S>,
    event: RecorderEvent,
) -> Result<bool, ControllerError> {
    match event {
        RecorderEvent::MediaRootResolved { path, fell_back } => {
            let replacement = MediaRootSnapshot { path, fell_back };
            if snapshot.media_root.as_ref() == Some(&replacement) {
                Ok(false)
            } else {
                snapshot.media_root = Some(replacement);
                Ok(true)
            }
        }
        RecorderEvent::Status {
            recording,
            waiting_for_game,
            segments,
            buffered_s,
            buffered_mb,
            full_session,
            encoder,
            capture_backend,
        } => {
            if !buffered_s.is_finite()
                || buffered_s < 0.0
                || !buffered_mb.is_finite()
                || buffered_mb < 0.0
            {
                return Err(ControllerError::InvalidRecorderMetric);
            }
            let replacement = RecorderStatus {
                recording,
                waiting_for_game,
                segments,
                buffered_s,
                buffered_mb,
                full_session,
                encoder,
                capture_backend,
            };
            if replacement == snapshot.recorder.status {
                Ok(false)
            } else {
                snapshot.recorder.status = replacement;
                Ok(true)
            }
        }
        RecorderEvent::Saved {
            path,
            seconds,
            recording_start_unix,
            recording_end_unix,
            markers,
            full_session,
            gc_deleted,
            gc_freed_bytes,
            storage_total_bytes,
            storage_quota_bytes,
            storage_over_quota,
        } => {
            if !seconds.is_finite() || seconds < 0.0 {
                return Err(ControllerError::InvalidRecorderMetric);
            }
            snapshot.latest_saved = Some(SavedReplay {
                path,
                seconds,
                recording_start_unix,
                recording_end_unix,
                markers,
                full_session,
                gc_deleted,
                gc_freed_bytes,
            });
            snapshot.storage = Some(StorageStatus {
                total_bytes: storage_total_bytes,
                quota_bytes: storage_quota_bytes,
                over_quota: storage_over_quota,
            });
            snapshot.library_revision = snapshot.library_revision.checked_next()?;
            Ok(true)
        }
        RecorderEvent::Error { message } => {
            push_notice(snapshot, NoticeKind::Error, message, None)?;
            Ok(true)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CloudProgressOutcome {
    Unchanged,
    BytesChanged,
    StateChanged,
    Stale,
}

fn apply_cloud_progress<S>(
    snapshot: &mut DesktopSnapshot<S>,
    account: CloudAccountOwner,
    generation: Generation,
    update: CloudUploadUpdateKind,
    progress: crate::CloudUploadProgress,
) -> Result<CloudProgressOutcome, ControllerError> {
    progress
        .validate_terminal_signal()
        .map_err(ControllerError::InvalidCloudProgress)?;
    match snapshot
        .uploads
        .binary_search_by(|upload| upload_order(upload, &account, &progress.local_clip_id))
    {
        Ok(index) => {
            let current = &snapshot.uploads[index];
            if generation < current.generation {
                return Ok(CloudProgressOutcome::Stale);
            }
            if update == CloudUploadUpdateKind::Bytes {
                if generation != current.generation {
                    return Err(ControllerError::InvalidCloudProgress(
                        "byte-only progress cannot start a new upload generation",
                    ));
                }
                if !same_cloud_state(&current.progress, &progress) {
                    return Err(ControllerError::InvalidCloudProgress(
                        "byte-only progress changed upload state",
                    ));
                }
                if current.progress.received_size_bytes == progress.received_size_bytes
                    && current.progress.file_size_bytes == progress.file_size_bytes
                {
                    return Ok(CloudProgressOutcome::Unchanged);
                }
                snapshot.uploads[index].progress.received_size_bytes = progress.received_size_bytes;
                snapshot.uploads[index].progress.file_size_bytes = progress.file_size_bytes;
                return Ok(CloudProgressOutcome::BytesChanged);
            }
            let replacement = CloudUploadSnapshot {
                account,
                generation,
                progress,
            };
            if replacement == *current {
                Ok(CloudProgressOutcome::Unchanged)
            } else {
                snapshot.uploads[index] = replacement;
                Ok(CloudProgressOutcome::StateChanged)
            }
        }
        Err(mut index) => {
            if update == CloudUploadUpdateKind::Bytes {
                return Err(ControllerError::InvalidCloudProgress(
                    "byte-only progress has no upload state",
                ));
            }
            if snapshot.uploads.len() >= MAX_ACTIVE_UPLOADS {
                let eviction_index = snapshot
                    .uploads
                    .iter()
                    .enumerate()
                    .filter(|(_, upload)| upload.progress.is_terminal())
                    .min_by_key(|(_, upload)| upload.generation)
                    .map(|(index, _)| index)
                    .ok_or(ControllerError::UploadsFull {
                        capacity: MAX_ACTIVE_UPLOADS,
                    })?;
                snapshot.uploads.remove(eviction_index);
                index = snapshot
                    .uploads
                    .binary_search_by(|upload| {
                        upload_order(upload, &account, &progress.local_clip_id)
                    })
                    .expect_err("the new upload was not present before insertion");
            }
            snapshot.uploads.insert(
                index,
                CloudUploadSnapshot {
                    account,
                    generation,
                    progress,
                },
            );
            Ok(CloudProgressOutcome::StateChanged)
        }
    }
}

fn same_cloud_state(
    current: &crate::CloudUploadProgress,
    next: &crate::CloudUploadProgress,
) -> bool {
    current.local_clip_id == next.local_clip_id
        && current.path == next.path
        && current.upload_status == next.upload_status
        && current.terminal == next.terminal
        && current.remote_clip_id == next.remote_clip_id
        && current.remote_url == next.remote_url
        && current.error == next.error
}

fn upload_order(
    upload: &CloudUploadSnapshot,
    account: &CloudAccountOwner,
    local_clip_id: &str,
) -> std::cmp::Ordering {
    upload
        .account
        .cmp(account)
        .then_with(|| upload.progress.local_clip_id.as_str().cmp(local_clip_id))
}

fn microphone_event_is_stale(snapshot: &MicrophoneSnapshot, generation: Generation) -> bool {
    generation < snapshot.generation
        || (generation == snapshot.generation
            && matches!(
                snapshot.phase,
                MicrophonePhase::Stopped | MicrophonePhase::Failed
            )
            && snapshot.generation != Generation::INITIAL)
}

fn push_notice<S>(
    snapshot: &mut DesktopSnapshot<S>,
    kind: NoticeKind,
    message: String,
    account: Option<CloudAccountOwner>,
) -> Result<(), ControllerError> {
    validate_notice_message(&message)?;
    if snapshot.notices.len() >= MAX_PENDING_NOTICES {
        return Err(ControllerError::NoticesFull {
            capacity: MAX_PENDING_NOTICES,
        });
    }
    let id = snapshot
        .notice_sequence
        .checked_add(1)
        .ok_or(ControllerError::GenerationExhausted)?;
    snapshot.notice_sequence = id;
    snapshot.notices.push(Notice {
        id,
        kind,
        message,
        created_revision: snapshot.revision.checked_next()?,
        account,
    });
    Ok(())
}

fn validate_notice_message(message: &str) -> Result<(), ControllerError> {
    if message.trim().is_empty() {
        return Err(ControllerError::NoticeEmpty);
    }
    if message.len() > MAX_NOTICE_MESSAGE_BYTES {
        return Err(ControllerError::NoticeTooLarge {
            actual: message.len(),
            maximum: MAX_NOTICE_MESSAGE_BYTES,
        });
    }
    Ok(())
}

fn validate_snapshot<S>(snapshot: &DesktopSnapshot<S>) -> Result<(), ControllerError> {
    if snapshot.schema_version != DESKTOP_SNAPSHOT_SCHEMA_VERSION {
        return Err(ControllerError::InvalidSnapshot(
            "unsupported schema version",
        ));
    }
    if snapshot.notices.len() > MAX_PENDING_NOTICES {
        return Err(ControllerError::InvalidSnapshot("too many notices"));
    }
    if snapshot
        .notices
        .iter()
        .any(|notice| validate_notice_message(&notice.message).is_err())
    {
        return Err(ControllerError::InvalidSnapshot(
            "notice message is empty or exceeds its byte bound",
        ));
    }
    if snapshot.uploads.len() > MAX_ACTIVE_UPLOADS {
        return Err(ControllerError::InvalidSnapshot("too many uploads"));
    }
    if snapshot
        .uploads
        .iter()
        .any(|upload| upload.progress.validate_terminal_signal().is_err())
    {
        return Err(ControllerError::InvalidSnapshot(
            "upload terminal signal is inconsistent with its status",
        ));
    }
    if snapshot
        .current_cloud_account
        .as_ref()
        .is_some_and(|account| account.account_generation() != snapshot.cloud_account_generation)
    {
        return Err(ControllerError::InvalidSnapshot(
            "cloud account generation is inconsistent",
        ));
    }
    if snapshot.notices.iter().any(|notice| {
        notice.id > snapshot.notice_sequence
            || notice
                .account
                .as_ref()
                .is_some_and(|account| snapshot.current_cloud_account.as_ref() != Some(account))
    }) {
        return Err(ControllerError::InvalidSnapshot("notice sequence is stale"));
    }
    if snapshot
        .notices
        .windows(2)
        .any(|pair| pair[0].id >= pair[1].id)
    {
        return Err(ControllerError::InvalidSnapshot(
            "notice identifiers are not unique and ordered",
        ));
    }
    if snapshot
        .uploads
        .iter()
        .any(|upload| snapshot.current_cloud_account.as_ref() != Some(&upload.account))
    {
        return Err(ControllerError::InvalidSnapshot(
            "upload belongs to a replaced cloud account",
        ));
    }
    if snapshot.uploads.windows(2).any(|pair| {
        upload_order(&pair[0], &pair[1].account, &pair[1].progress.local_clip_id)
            != std::cmp::Ordering::Less
    }) {
        return Err(ControllerError::InvalidSnapshot(
            "uploads are not uniquely sorted",
        ));
    }
    Ok(())
}
