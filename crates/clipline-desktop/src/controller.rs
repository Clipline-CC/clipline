use thiserror::Error;

use crate::{
    CloudUploadSnapshot, DesktopSnapshot, GameSnapshot, Generation, GenerationError,
    MediaRootSnapshot, MicrophonePhase, MicrophoneSnapshot, Notice, NoticeKind, RecorderEvent,
    RecorderSnapshot, RecorderStatus, Revision, SavedReplay, StorageStatus, UiAction, UiEffect,
    UiEvent, WindowLifecycleSnapshot,
};

pub const MAX_PENDING_NOTICES: usize = 64;
pub const MAX_ACTIVE_UPLOADS: usize = 16;
pub const DESKTOP_SNAPSHOT_SCHEMA_VERSION: u32 = 1;

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
                uploads: Vec::new(),
                library_revision: Revision::INITIAL,
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

    pub fn dispatch(&mut self, action: UiAction) -> Result<DispatchOutcome, ControllerError> {
        let effect = action.effect();
        let mut changed = false;
        if let UiAction::AcknowledgeNotice { notice_id } = action {
            if let Some(index) = self
                .snapshot
                .notices
                .iter()
                .position(|notice| notice.id == notice_id)
            {
                let revision = self.snapshot.revision.checked_next()?;
                self.snapshot.notices.remove(index);
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
            UiEvent::CloudUploadProgress {
                generation,
                progress,
            } => {
                if next
                    .uploads
                    .binary_search_by(|upload| {
                        upload.progress.local_clip_id.cmp(&progress.local_clip_id)
                    })
                    .is_ok_and(|index| generation < next.uploads[index].generation)
                {
                    return Ok(ApplyEventOutcome::Stale);
                }
                apply_cloud_progress(&mut next, generation, progress)?
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
            UiEvent::UserError { message } => {
                push_notice(&mut next, NoticeKind::Error, message)?;
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
            push_notice(snapshot, NoticeKind::Error, message)?;
            Ok(true)
        }
    }
}

fn apply_cloud_progress<S>(
    snapshot: &mut DesktopSnapshot<S>,
    generation: Generation,
    progress: crate::CloudUploadProgress,
) -> Result<bool, ControllerError> {
    match snapshot
        .uploads
        .binary_search_by(|upload| upload.progress.local_clip_id.cmp(&progress.local_clip_id))
    {
        Ok(index) => {
            let current = &snapshot.uploads[index];
            if generation < current.generation {
                return Ok(false);
            }
            let replacement = CloudUploadSnapshot {
                generation,
                progress,
            };
            if replacement == *current {
                Ok(false)
            } else {
                snapshot.uploads[index] = replacement;
                Ok(true)
            }
        }
        Err(index) => {
            if snapshot.uploads.len() >= MAX_ACTIVE_UPLOADS {
                return Err(ControllerError::UploadsFull {
                    capacity: MAX_ACTIVE_UPLOADS,
                });
            }
            snapshot.uploads.insert(
                index,
                CloudUploadSnapshot {
                    generation,
                    progress,
                },
            );
            Ok(true)
        }
    }
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
) -> Result<(), ControllerError> {
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
    });
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
    if snapshot.uploads.len() > MAX_ACTIVE_UPLOADS {
        return Err(ControllerError::InvalidSnapshot("too many uploads"));
    }
    if snapshot
        .notices
        .iter()
        .any(|notice| notice.id > snapshot.notice_sequence)
    {
        return Err(ControllerError::InvalidSnapshot("notice sequence is stale"));
    }
    if snapshot
        .uploads
        .windows(2)
        .any(|pair| pair[0].progress.local_clip_id >= pair[1].progress.local_clip_id)
    {
        return Err(ControllerError::InvalidSnapshot(
            "uploads are not uniquely sorted",
        ));
    }
    Ok(())
}
