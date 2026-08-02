use std::collections::VecDeque;
use std::path::PathBuf;

use clipline_mp4::SeekPlan;
use thiserror::Error;

use crate::{
    AcceptedCommand, BackendError, CommandError, CommandInbox, EnqueueError, EnqueueOutcome,
    MonotonicTime100ns, PipelineToken, PlaybackCommand, PlaybackEvent, PlaybackSnapshot,
    PlaybackState, PlaybackTime, RecoveryDisposition, WorkGeneration,
};

pub const MAX_PIPELINE_RECOVERY_ATTEMPTS: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerSeekPlan {
    pub target: PlaybackTime,
    pub sync_sample_index: usize,
    pub target_sample_index: usize,
}

impl WorkerSeekPlan {
    pub fn new(
        target: PlaybackTime,
        sync_sample_index: usize,
        target_sample_index: usize,
    ) -> Result<Self, WorkerError> {
        if target.timescale == 0 {
            return Err(WorkerError::InvalidTime);
        }
        if sync_sample_index > target_sample_index {
            return Err(WorkerError::InvalidSeekPlan {
                sync_sample_index,
                target_sample_index,
            });
        }
        Ok(Self {
            target,
            sync_sample_index,
            target_sample_index,
        })
    }
}

impl TryFrom<&SeekPlan> for WorkerSeekPlan {
    type Error = WorkerError;

    fn try_from(plan: &SeekPlan) -> Result<Self, Self::Error> {
        if plan.video_sync_sample.track_index != plan.video_preroll.track_index {
            return Err(WorkerError::SeekTrackMismatch {
                sync_track_index: plan.video_sync_sample.track_index,
                preroll_track_index: plan.video_preroll.track_index,
            });
        }
        let target_sample_index = plan
            .video_preroll
            .samples
            .end
            .checked_sub(1)
            .filter(|target| *target >= plan.video_preroll.samples.start)
            .ok_or(WorkerError::EmptyVideoPreroll)?;
        Self::new(
            plan.target_time,
            plan.video_sync_sample.sample_index,
            target_sample_index,
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum WorkerActionKind {
    IndexOpen {
        path: PathBuf,
    },
    Flush,
    PlanSeek {
        requested: PlaybackTime,
        audio_track_indices: Vec<usize>,
        step_frames: Option<i32>,
        accepted_at: MonotonicTime100ns,
    },
    ReadVideo {
        sample_index: usize,
    },
    ConvertVideo {
        sample_index: usize,
    },
    DecodeVideo {
        sample_index: usize,
    },
    ProduceAudio,
    PublishVideo {
        sample_index: usize,
    },
    SetTransport {
        playing: bool,
    },
    SetVolume {
        volume: f32,
    },
    CloseBackends,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WorkerAction {
    id: u64,
    token: PipelineToken,
    kind: WorkerActionKind,
}

impl WorkerAction {
    pub const fn id(&self) -> u64 {
        self.id
    }

    pub const fn token(&self) -> PipelineToken {
        self.token
    }

    pub const fn kind(&self) -> &WorkerActionKind {
        &self.kind
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerCompletion {
    Indexed {
        duration: PlaybackTime,
        video_sample_count: usize,
        default_audio_track_indices: Vec<usize>,
    },
    SeekPlanned(WorkerSeekPlan),
    Published {
        position: PlaybackTime,
    },
    Done,
}

#[derive(Debug, Error)]
pub enum WorkerError {
    #[error(transparent)]
    Command(#[from] CommandError),
    #[error("worker time must have a non-zero timescale")]
    InvalidTime,
    #[error(
        "seek plan sync sample {sync_sample_index} is after target sample {target_sample_index}"
    )]
    InvalidSeekPlan {
        sync_sample_index: usize,
        target_sample_index: usize,
    },
    #[error("indexed seek plan has an empty video preroll")]
    EmptyVideoPreroll,
    #[error(
        "indexed seek sync track {sync_track_index} does not match preroll track {preroll_track_index}"
    )]
    SeekTrackMismatch {
        sync_track_index: usize,
        preroll_track_index: usize,
    },
    #[error("indexed media has no video samples")]
    EmptyVideo,
    #[error("indexed audio tracks cannot be installed while media is not opening")]
    InvalidOpenState,
    #[error(
        "seek target sample {target_sample_index} is outside {video_sample_count} video samples"
    )]
    TargetOutsideVideo {
        target_sample_index: usize,
        video_sample_count: usize,
    },
    #[error("worker action counter is exhausted")]
    ActionCounterExhausted,
    #[error("pipeline revision counter is exhausted")]
    RevisionExhausted,
    #[error("completion {actual} does not match worker stage {expected}")]
    UnexpectedCompletion {
        expected: &'static str,
        actual: &'static str,
    },
    #[error("published position {actual:?} does not match seek target {expected:?}")]
    WrongPublishedPosition {
        expected: PlaybackTime,
        actual: PlaybackTime,
    },
}

#[derive(Debug, Clone)]
enum OperationOrigin {
    Open { duration: PlaybackTime },
    UserSeek,
    Recovery,
}

#[derive(Debug, Clone)]
struct SeekOperation {
    origin: OperationOrigin,
    requested: PlaybackTime,
    step_frames: Option<i32>,
    accepted_at: MonotonicTime100ns,
}

#[derive(Debug, Clone)]
enum WorkerStage {
    Closed,
    IndexOpen {
        path: PathBuf,
        accepted_at: MonotonicTime100ns,
    },
    Flush(SeekOperation),
    PlanSeek(SeekOperation),
    ReadVideo {
        operation: SeekOperation,
        plan: WorkerSeekPlan,
        sample_index: usize,
    },
    ConvertVideo {
        operation: SeekOperation,
        plan: WorkerSeekPlan,
        sample_index: usize,
    },
    DecodeVideo {
        operation: SeekOperation,
        plan: WorkerSeekPlan,
        sample_index: usize,
    },
    ProduceAudio {
        operation: SeekOperation,
        plan: WorkerSeekPlan,
    },
    PublishVideo {
        operation: SeekOperation,
        plan: WorkerSeekPlan,
    },
    SetTransport {
        playing: bool,
    },
    SetVolume {
        volume: f32,
    },
    Ready,
    Close,
    Failed,
    Transition,
}

impl WorkerStage {
    fn name(&self) -> &'static str {
        match self {
            Self::Closed => "closed",
            Self::IndexOpen { .. } => "index open",
            Self::Flush(_) => "flush",
            Self::PlanSeek(_) => "seek plan",
            Self::ReadVideo { .. } => "video read",
            Self::ConvertVideo { .. } => "video conversion",
            Self::DecodeVideo { .. } => "video decode",
            Self::ProduceAudio { .. } => "audio production",
            Self::PublishVideo { .. } => "video publication",
            Self::SetTransport { .. } => "transport",
            Self::SetVolume { .. } => "volume",
            Self::Ready => "ready",
            Self::Close => "close",
            Self::Failed => "failed",
            Self::Transition => "transition",
        }
    }
}

#[derive(Debug)]
pub struct PlaybackWorker {
    state: PlaybackState,
    inbox: CommandInbox,
    token: PipelineToken,
    stage: WorkerStage,
    current_action: Option<WorkerAction>,
    next_action_id: u64,
    video_sample_count: usize,
    pending_transport: Option<bool>,
    pending_volume: Option<f32>,
    events: VecDeque<PlaybackEvent>,
    stale_completions: u64,
    recovery_attempts: usize,
}

impl Default for PlaybackWorker {
    fn default() -> Self {
        Self {
            state: PlaybackState::default(),
            inbox: CommandInbox::new(),
            token: PipelineToken::new(WorkGeneration::INITIAL, 0),
            stage: WorkerStage::Closed,
            current_action: None,
            next_action_id: 0,
            video_sample_count: 0,
            pending_transport: None,
            pending_volume: None,
            events: VecDeque::new(),
            stale_completions: 0,
            recovery_attempts: 0,
        }
    }
}

impl PlaybackWorker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn enqueue(
        &mut self,
        command: PlaybackCommand,
        accepted_at: MonotonicTime100ns,
    ) -> Result<EnqueueOutcome, EnqueueError> {
        self.inbox.enqueue_at(command, accepted_at)
    }

    pub fn next_action(&mut self) -> Result<Option<WorkerAction>, WorkerError> {
        self.drain_commands()?;
        if self.current_action.is_some() {
            return Ok(None);
        }
        self.promote_pending_control();
        let kind = match &self.stage {
            WorkerStage::IndexOpen { path, .. } => {
                WorkerActionKind::IndexOpen { path: path.clone() }
            }
            WorkerStage::Flush(_) => WorkerActionKind::Flush,
            WorkerStage::PlanSeek(operation) => WorkerActionKind::PlanSeek {
                requested: operation.requested,
                audio_track_indices: self.state.snapshot().audio_track_indices,
                step_frames: operation.step_frames,
                accepted_at: operation.accepted_at,
            },
            WorkerStage::ReadVideo { sample_index, .. } => WorkerActionKind::ReadVideo {
                sample_index: *sample_index,
            },
            WorkerStage::ConvertVideo { sample_index, .. } => WorkerActionKind::ConvertVideo {
                sample_index: *sample_index,
            },
            WorkerStage::DecodeVideo { sample_index, .. } => WorkerActionKind::DecodeVideo {
                sample_index: *sample_index,
            },
            WorkerStage::ProduceAudio { .. } => WorkerActionKind::ProduceAudio,
            WorkerStage::PublishVideo { plan, .. } => WorkerActionKind::PublishVideo {
                sample_index: plan.target_sample_index,
            },
            WorkerStage::SetTransport { playing } => {
                WorkerActionKind::SetTransport { playing: *playing }
            }
            WorkerStage::SetVolume { volume } => WorkerActionKind::SetVolume { volume: *volume },
            WorkerStage::Close => WorkerActionKind::CloseBackends,
            WorkerStage::Closed
            | WorkerStage::Ready
            | WorkerStage::Failed
            | WorkerStage::Transition => return Ok(None),
        };
        let id = self
            .next_action_id
            .checked_add(1)
            .ok_or(WorkerError::ActionCounterExhausted)?;
        self.next_action_id = id;
        let action = WorkerAction {
            id,
            token: self.token,
            kind,
        };
        self.current_action = Some(action.clone());
        Ok(Some(action))
    }

    pub fn complete(
        &mut self,
        action: &WorkerAction,
        completion: WorkerCompletion,
    ) -> Result<bool, WorkerError> {
        if !self.accepts_action(action) {
            self.stale_completions = self.stale_completions.saturating_add(1);
            return Ok(false);
        }
        self.current_action = None;
        let stage = std::mem::replace(&mut self.stage, WorkerStage::Transition);
        self.stage = match (stage, completion) {
            (
                WorkerStage::IndexOpen { accepted_at, .. },
                WorkerCompletion::Indexed {
                    duration,
                    video_sample_count,
                    default_audio_track_indices,
                },
            ) => {
                if duration.timescale == 0 {
                    return self.completion_error(WorkerError::InvalidTime);
                }
                if video_sample_count == 0 {
                    return self.completion_error(WorkerError::EmptyVideo);
                }
                let next_token = match self.token.next_revision() {
                    Some(token) => token,
                    None => return self.completion_error(WorkerError::RevisionExhausted),
                };
                match self
                    .state
                    .install_open_audio_tracks(self.token.work(), default_audio_track_indices)
                {
                    Ok(true) => {}
                    Ok(false) => {
                        return self.completion_error(WorkerError::InvalidOpenState);
                    }
                    Err(error) => return self.completion_error(error.into()),
                }
                self.video_sample_count = video_sample_count;
                self.token = next_token;
                WorkerStage::Flush(SeekOperation {
                    origin: OperationOrigin::Open { duration },
                    requested: PlaybackTime {
                        ticks: 0,
                        timescale: 1,
                    },
                    step_frames: None,
                    accepted_at,
                })
            }
            (WorkerStage::Flush(operation), WorkerCompletion::Done) => {
                WorkerStage::PlanSeek(operation)
            }
            (WorkerStage::PlanSeek(operation), WorkerCompletion::SeekPlanned(plan)) => {
                if plan.target_sample_index >= self.video_sample_count {
                    return self.completion_error(WorkerError::TargetOutsideVideo {
                        target_sample_index: plan.target_sample_index,
                        video_sample_count: self.video_sample_count,
                    });
                }
                let sample_index = plan.sync_sample_index;
                WorkerStage::ReadVideo {
                    operation,
                    plan,
                    sample_index,
                }
            }
            (
                WorkerStage::ReadVideo {
                    operation,
                    plan,
                    sample_index,
                },
                WorkerCompletion::Done,
            ) => WorkerStage::ConvertVideo {
                operation,
                plan,
                sample_index,
            },
            (
                WorkerStage::ConvertVideo {
                    operation,
                    plan,
                    sample_index,
                },
                WorkerCompletion::Done,
            ) => WorkerStage::DecodeVideo {
                operation,
                plan,
                sample_index,
            },
            (
                WorkerStage::DecodeVideo {
                    operation,
                    plan,
                    sample_index,
                },
                WorkerCompletion::Done,
            ) => {
                if sample_index < plan.target_sample_index {
                    WorkerStage::ReadVideo {
                        operation,
                        plan,
                        sample_index: sample_index + 1,
                    }
                } else {
                    WorkerStage::ProduceAudio { operation, plan }
                }
            }
            (WorkerStage::ProduceAudio { operation, plan }, WorkerCompletion::Done) => {
                WorkerStage::PublishVideo { operation, plan }
            }
            (
                WorkerStage::PublishVideo { operation, plan },
                WorkerCompletion::Published { position },
            ) => {
                if !same_time(position, plan.target) {
                    return self.completion_error(WorkerError::WrongPublishedPosition {
                        expected: plan.target,
                        actual: position,
                    });
                }
                if let Err(error) = self.finish_operation(operation, position) {
                    return self.completion_error(error);
                }
                WorkerStage::Ready
            }
            (WorkerStage::SetTransport { .. }, WorkerCompletion::Done)
            | (WorkerStage::SetVolume { .. }, WorkerCompletion::Done) => WorkerStage::Ready,
            (WorkerStage::Close, WorkerCompletion::Done) => {
                self.events.push_back(PlaybackEvent::Closed {
                    generation: self.token.work(),
                });
                WorkerStage::Closed
            }
            (stage, completion) => {
                return self.completion_error(WorkerError::UnexpectedCompletion {
                    expected: stage.name(),
                    actual: completion.name(),
                });
            }
        };
        Ok(true)
    }

    pub fn fail(
        &mut self,
        action: &WorkerAction,
        error: BackendError,
    ) -> Result<bool, WorkerError> {
        if !self.accepts_action(action) {
            self.stale_completions = self.stale_completions.saturating_add(1);
            return Ok(false);
        }
        self.current_action = None;
        self.handle_backend_failure(error)?;
        Ok(true)
    }

    pub fn report_failure(
        &mut self,
        token: PipelineToken,
        error: BackendError,
    ) -> Result<bool, WorkerError> {
        if token != self.token
            || !matches!(self.stage, WorkerStage::Ready)
            || self.current_action.is_some()
        {
            self.stale_completions = self.stale_completions.saturating_add(1);
            return Ok(false);
        }
        self.handle_backend_failure(error)?;
        Ok(true)
    }

    fn handle_backend_failure(&mut self, error: BackendError) -> Result<(), WorkerError> {
        if error.recovery == RecoveryDisposition::Fatal
            || self.recovery_attempts >= MAX_PIPELINE_RECOVERY_ATTEMPTS
        {
            self.fail_terminal(error.message);
            return Ok(());
        }

        self.recovery_attempts += 1;
        let recovery_stage = self.recovery_stage()?;
        if matches!(recovery_stage, WorkerStage::Failed) {
            self.fail_terminal(error.message);
        } else {
            self.stage = recovery_stage;
        }
        Ok(())
    }

    pub fn snapshot(&self) -> PlaybackSnapshot {
        self.state.snapshot()
    }

    pub const fn token(&self) -> PipelineToken {
        self.token
    }

    pub const fn stale_completions(&self) -> u64 {
        self.stale_completions
    }

    pub const fn recovery_attempts(&self) -> usize {
        self.recovery_attempts
    }

    pub fn take_events(&mut self) -> Vec<PlaybackEvent> {
        self.events.drain(..).collect()
    }

    pub fn report_position(&mut self, token: PipelineToken, position: PlaybackTime) -> bool {
        if token != self.token || !matches!(self.stage, WorkerStage::Ready) {
            return false;
        }
        self.state.update_position(token.work(), position)
    }

    pub fn report_ended(&mut self, token: PipelineToken, position: PlaybackTime) -> bool {
        if token != self.token || !matches!(self.stage, WorkerStage::Ready) {
            return false;
        }
        if !self.state.mark_ended(token.work(), position) {
            return false;
        }
        self.pending_transport = None;
        self.events.push_back(PlaybackEvent::Ended {
            generation: token.work(),
        });
        true
    }

    fn drain_commands(&mut self) -> Result<(), WorkerError> {
        while let Some(AcceptedCommand {
            command,
            accepted_at,
        }) = self.inbox.pop_front_accepted()
        {
            let before = self.state.snapshot();
            let generation = self.state.apply(command.clone())?;
            let generation_changed = generation != before.generation;
            match command {
                PlaybackCommand::Open { path } => {
                    self.token = PipelineToken::new(generation, 0);
                    self.stage = WorkerStage::IndexOpen { path, accepted_at };
                    self.invalidate_action();
                    self.video_sample_count = 0;
                    self.recovery_attempts = 0;
                    self.pending_transport = None;
                }
                PlaybackCommand::Close => {
                    self.token = PipelineToken::new(generation, 0);
                    self.stage = WorkerStage::Close;
                    self.invalidate_action();
                    self.pending_transport = None;
                    self.pending_volume = None;
                }
                PlaybackCommand::Seek { position } => {
                    self.start_user_seek(generation, position, None, accepted_at)?;
                }
                PlaybackCommand::Step { frames } => {
                    self.start_user_seek(generation, before.position, Some(frames), accepted_at)?;
                }
                PlaybackCommand::SetTracks { .. } if generation_changed => {
                    self.start_user_seek(generation, before.position, None, accepted_at)?;
                }
                PlaybackCommand::SetTracks { .. } => {}
                PlaybackCommand::SetVolume { volume } => {
                    self.pending_volume = Some(volume);
                }
                PlaybackCommand::Play => self.pending_transport = Some(true),
                PlaybackCommand::Pause => self.pending_transport = Some(false),
                PlaybackCommand::SetRate { .. } => {}
            }
        }
        Ok(())
    }

    fn start_user_seek(
        &mut self,
        generation: WorkGeneration,
        requested: PlaybackTime,
        step_frames: Option<i32>,
        accepted_at: MonotonicTime100ns,
    ) -> Result<(), WorkerError> {
        self.token = PipelineToken::new(generation, 0)
            .next_revision()
            .ok_or(WorkerError::RevisionExhausted)?;
        self.stage = WorkerStage::Flush(SeekOperation {
            origin: OperationOrigin::UserSeek,
            requested,
            step_frames,
            accepted_at,
        });
        self.invalidate_action();
        self.recovery_attempts = 0;
        Ok(())
    }

    fn finish_operation(
        &mut self,
        operation: SeekOperation,
        position: PlaybackTime,
    ) -> Result<(), WorkerError> {
        let generation = self.token.work();
        match operation.origin {
            OperationOrigin::Open { duration } => {
                if !self.state.complete_open(generation, duration)
                    || !self.state.update_position(generation, position)
                {
                    return Err(WorkerError::InvalidTime);
                }
                self.events.push_back(PlaybackEvent::Opened {
                    generation,
                    duration,
                });
            }
            OperationOrigin::UserSeek => {
                if !self.state.complete_seek(generation, position) {
                    return Err(WorkerError::InvalidTime);
                }
                self.events.push_back(PlaybackEvent::SeekSettled {
                    generation,
                    position,
                });
            }
            OperationOrigin::Recovery => {
                if !self.state.complete_seek(generation, position) {
                    return Err(WorkerError::InvalidTime);
                }
            }
        }
        self.recovery_attempts = 0;
        Ok(())
    }

    fn promote_pending_control(&mut self) {
        if !matches!(self.stage, WorkerStage::Ready) {
            return;
        }
        if let Some(volume) = self.pending_volume.take() {
            self.stage = WorkerStage::SetVolume { volume };
        } else if let Some(playing) = self.pending_transport.take() {
            self.stage = WorkerStage::SetTransport { playing };
        }
    }

    fn accepts_action(&self, action: &WorkerAction) -> bool {
        self.current_action
            .as_ref()
            .is_some_and(|current| current == action)
            && action.token == self.token
    }

    fn invalidate_action(&mut self) {
        self.current_action = None;
    }

    fn bump_revision(&mut self) -> Result<(), WorkerError> {
        self.token = self
            .token
            .next_revision()
            .ok_or(WorkerError::RevisionExhausted)?;
        Ok(())
    }

    fn recovery_stage(&mut self) -> Result<WorkerStage, WorkerError> {
        let prior = self.stage.clone();
        self.bump_revision()?;
        match prior {
            WorkerStage::IndexOpen { path, accepted_at } => {
                Ok(WorkerStage::IndexOpen { path, accepted_at })
            }
            WorkerStage::Flush(operation)
            | WorkerStage::PlanSeek(operation)
            | WorkerStage::ReadVideo { operation, .. }
            | WorkerStage::ConvertVideo { operation, .. }
            | WorkerStage::DecodeVideo { operation, .. }
            | WorkerStage::ProduceAudio { operation, .. }
            | WorkerStage::PublishVideo { operation, .. } => Ok(WorkerStage::Flush(operation)),
            WorkerStage::Ready
            | WorkerStage::SetTransport { .. }
            | WorkerStage::SetVolume { .. } => {
                let position = self.state.snapshot().position;
                if !self.state.begin_recovery_seek(self.token.work(), position) {
                    return Ok(WorkerStage::Failed);
                }
                Ok(WorkerStage::Flush(SeekOperation {
                    origin: OperationOrigin::Recovery,
                    requested: position,
                    step_frames: None,
                    accepted_at: MonotonicTime100ns::new(0),
                }))
            }
            WorkerStage::Close
            | WorkerStage::Closed
            | WorkerStage::Failed
            | WorkerStage::Transition => Ok(WorkerStage::Failed),
        }
    }

    fn fail_terminal(&mut self, message: String) {
        let generation = self.token.work();
        self.state.fail(generation);
        self.events.push_back(PlaybackEvent::Error {
            generation,
            message,
        });
        self.stage = WorkerStage::Failed;
    }

    fn completion_error<T>(&mut self, error: WorkerError) -> Result<T, WorkerError> {
        self.fail_terminal(error.to_string());
        Err(error)
    }
}

impl WorkerCompletion {
    fn name(&self) -> &'static str {
        match self {
            Self::Indexed { .. } => "indexed",
            Self::SeekPlanned(_) => "seek planned",
            Self::Published { .. } => "published",
            Self::Done => "done",
        }
    }
}

fn same_time(left: PlaybackTime, right: PlaybackTime) -> bool {
    u128::from(left.ticks) * u128::from(right.timescale)
        == u128::from(right.ticks) * u128::from(left.timescale)
}
