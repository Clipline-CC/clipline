use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use thiserror::Error;

use crate::{
    CatalogSummarySnapshot, CloudAccountOwner, CloudAccountScope, CloudUploadUpdateKind,
    Generation, ProbeKind, ProbePhase, ProbeSessionOwner, ProbeSummary, ProbeToken, RecorderEvent,
    Revision, UiEvent, WindowLifecycleMode,
};

pub const UI_EVENT_CAPACITY: usize = 128;
/// Fixed terminal headroom beyond the legacy 128-event producer budget.
/// This keeps one microphone Error -> Stopped pair admissible even when an
/// existing frontend has filled the compatibility queue completely.
pub const UI_EVENT_MAX_BUFFERED: usize = UI_EVENT_CAPACITY + 2;

#[derive(Debug, Clone, PartialEq)]
pub struct SequencedUiEvent {
    pub sequence: u64,
    pub event: UiEvent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiEventPublishOutcome {
    Queued,
    Replaced,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum UiEventSendError {
    #[error("UI event consumer is disconnected")]
    Disconnected,
    #[error("UI event queue is full at capacity {capacity}")]
    Full { capacity: usize },
    #[error("stale UI event generation {received:?}; current is {current:?}")]
    Stale {
        current: Generation,
        received: Generation,
    },
    #[error("stale UI lifecycle revision {received:?}; current is {current:?}")]
    StaleRevision {
        current: Revision,
        received: Revision,
    },
    #[error("UI event belongs to a replaced cloud account")]
    AccountChanged,
    #[error("stale cloud account generation {received:?}; current is {current:?}")]
    StaleAccount {
        current: CloudAccountScope,
        received: CloudAccountScope,
    },
    #[error("cloud account owner generation does not match its event")]
    InvalidAccountGeneration,
    #[error("invalid cloud upload progress: {0}")]
    InvalidCloudProgress(&'static str),
    #[error("invalid settings probe summary")]
    InvalidProbeSummary,
    #[error("stale settings probe token {received:?}; current is {current:?}")]
    StaleProbe {
        current: ProbeToken,
        received: ProbeToken,
    },
    #[error("stale settings probe owner {received:?}; current is {current:?}")]
    StaleProbeOwner {
        current: ProbeSessionOwner,
        received: ProbeSessionOwner,
    },
    #[error("UI event sequence is exhausted")]
    SequenceExhausted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum UiEventReceiveError {
    #[error("all UI event producers are disconnected")]
    Disconnected,
}

pub trait UiEventSink: Send + Sync {
    fn try_publish(&self, event: UiEvent) -> Result<UiEventPublishOutcome, UiEventSendError>;
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum GenerationDomain {
    Recorder,
    Microphone,
    GameDetection,
    CloudUpload(CloudAccountOwner, String),
    Enrichment,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CoalescingKey {
    RecorderStatus,
    MediaRoot,
    WindowLifecycle,
    MicrophoneMonitor,
    GameDetection,
    CloudUpload(CloudAccountOwner, String),
    Enrichment,
    CatalogSummary,
}

#[derive(Default)]
struct ChannelState {
    queue: VecDeque<SequencedUiEvent>,
    next_sequence: u64,
    receiver_connected: bool,
    sender_count: usize,
    generations: HashMap<GenerationDomain, Generation>,
    microphone_error: Option<Generation>,
    microphone_terminal: Option<Generation>,
    lifecycle: Option<(Revision, WindowLifecycleMode)>,
    catalog: Option<CatalogSummarySnapshot>,
    cloud_account_generation: CloudAccountScope,
    cloud_account: Option<CloudAccountOwner>,
    probe_owner: Option<ProbeSessionOwner>,
    probe_summaries: HashMap<ProbeKind, ProbeSummary>,
}

struct Shared {
    state: Mutex<ChannelState>,
    ready: Condvar,
}

pub struct UiEventSender {
    shared: Arc<Shared>,
}

pub struct UiEventReceiver {
    shared: Arc<Shared>,
}

#[must_use]
pub fn ui_event_channel() -> (UiEventSender, UiEventReceiver) {
    let shared = Arc::new(Shared {
        state: Mutex::new(ChannelState {
            receiver_connected: true,
            sender_count: 1,
            ..ChannelState::default()
        }),
        ready: Condvar::new(),
    });
    (
        UiEventSender {
            shared: Arc::clone(&shared),
        },
        UiEventReceiver { shared },
    )
}

impl Clone for UiEventSender {
    fn clone(&self) -> Self {
        if let Ok(mut state) = self.shared.state.lock() {
            state.sender_count = state.sender_count.saturating_add(1);
        }
        Self {
            shared: Arc::clone(&self.shared),
        }
    }
}

impl UiEventSink for UiEventSender {
    fn try_publish(&self, event: UiEvent) -> Result<UiEventPublishOutcome, UiEventSendError> {
        UiEventSender::try_publish(self, event)
    }
}

impl UiEventSender {
    pub fn try_publish(&self, event: UiEvent) -> Result<UiEventPublishOutcome, UiEventSendError> {
        let mut state = self
            .shared
            .state
            .lock()
            .map_err(|_| UiEventSendError::Disconnected)?;
        if !state.receiver_connected {
            return Err(UiEventSendError::Disconnected);
        }
        validate_generation(&state, &event)?;

        let replacement = coalescing_key(&event).and_then(|key| {
            state
                .queue
                .iter()
                .enumerate()
                .rev()
                .take_while(|(_, queued)| !is_durable_barrier(&queued.event))
                .find_map(|(index, queued)| {
                    (coalescing_key(&queued.event).as_ref() == Some(&key)).then_some(index)
                })
        });
        let limit = queue_limit(&event);
        if replacement.is_none() && state.queue.len() >= limit {
            return Err(UiEventSendError::Full {
                capacity: UI_EVENT_CAPACITY,
            });
        }
        let sequence = state
            .next_sequence
            .checked_add(1)
            .ok_or(UiEventSendError::SequenceExhausted)?;

        record_generation(&mut state, &event);
        state.next_sequence = sequence;
        let update = SequencedUiEvent { sequence, event };
        let outcome = if let Some(index) = replacement {
            state.queue.remove(index);
            state.queue.push_back(update);
            UiEventPublishOutcome::Replaced
        } else {
            state.queue.push_back(update);
            UiEventPublishOutcome::Queued
        };
        drop(state);
        self.shared.ready.notify_one();
        Ok(outcome)
    }
}

impl Drop for UiEventSender {
    fn drop(&mut self) {
        if let Ok(mut state) = self.shared.state.lock() {
            state.sender_count = state.sender_count.saturating_sub(1);
        }
        self.shared.ready.notify_all();
    }
}

impl UiEventReceiver {
    #[must_use]
    pub fn try_recv(&self) -> Option<SequencedUiEvent> {
        self.shared
            .state
            .lock()
            .ok()
            .and_then(|mut state| state.queue.pop_front())
    }

    pub fn wait_recv(
        &self,
        timeout: Duration,
    ) -> Result<Option<SequencedUiEvent>, UiEventReceiveError> {
        let mut state = self
            .shared
            .state
            .lock()
            .map_err(|_| UiEventReceiveError::Disconnected)?;
        if state.queue.is_empty() && state.sender_count != 0 {
            state = self
                .shared
                .ready
                .wait_timeout(state, timeout)
                .map_err(|_| UiEventReceiveError::Disconnected)?
                .0;
        }
        if let Some(update) = state.queue.pop_front() {
            Ok(Some(update))
        } else if state.sender_count == 0 {
            Err(UiEventReceiveError::Disconnected)
        } else {
            Ok(None)
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.shared
            .state
            .lock()
            .map_or(0, |state| state.queue.len())
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Drop for UiEventReceiver {
    fn drop(&mut self) {
        if let Ok(mut state) = self.shared.state.lock() {
            state.receiver_connected = false;
            state.queue.clear();
        }
        self.shared.ready.notify_all();
    }
}

fn generation_domain(event: &UiEvent) -> Option<(GenerationDomain, Generation)> {
    match event {
        UiEvent::Recorder { generation, .. } => Some((GenerationDomain::Recorder, *generation)),
        UiEvent::MicMonitor { generation, .. }
        | UiEvent::MicTestError { generation, .. }
        | UiEvent::MicTestStopped { generation } => {
            Some((GenerationDomain::Microphone, *generation))
        }
        UiEvent::GameDetection { generation, .. } => {
            Some((GenerationDomain::GameDetection, *generation))
        }
        UiEvent::CloudUploadProgress {
            account,
            generation,
            progress,
            ..
        } => Some((
            GenerationDomain::CloudUpload(account.clone(), progress.local_clip_id.clone()),
            *generation,
        )),
        UiEvent::CloudUploadRemoved {
            account,
            generation,
            local_clip_id,
        } => Some((
            GenerationDomain::CloudUpload(account.clone(), local_clip_id.clone()),
            *generation,
        )),
        UiEvent::EnrichmentUpdated { generation } => {
            Some((GenerationDomain::Enrichment, *generation))
        }
        UiEvent::WindowLifecycle { .. }
        | UiEvent::CatalogSummaryChanged { .. }
        | UiEvent::SettingsProbeChanged { .. }
        | UiEvent::CloudAccountChanged { .. }
        | UiEvent::UserError { .. } => None,
    }
}

fn validate_generation(state: &ChannelState, event: &UiEvent) -> Result<(), UiEventSendError> {
    if let UiEvent::CloudUploadProgress { progress, .. } = event {
        progress
            .validate_terminal_signal()
            .map_err(UiEventSendError::InvalidCloudProgress)?;
    }
    if let UiEvent::WindowLifecycle { snapshot } = event {
        if let Some((current, mode)) = state.lifecycle {
            if snapshot.revision < current
                || (snapshot.revision == current && snapshot.mode != mode)
            {
                return Err(UiEventSendError::StaleRevision {
                    current,
                    received: snapshot.revision,
                });
            }
        }
    }
    if let UiEvent::CatalogSummaryChanged { summary } = event {
        if let Some(current) = state.catalog {
            if summary.revision < current.revision
                || (summary.revision == current.revision && *summary != current)
            {
                return Err(UiEventSendError::StaleRevision {
                    current: current.revision,
                    received: summary.revision,
                });
            }
        }
    }
    if let UiEvent::SettingsProbeChanged { summary } = event {
        if summary.phase == ProbePhase::Pending || summary.validate().is_err() {
            return Err(UiEventSendError::InvalidProbeSummary);
        }
        if let Some(current) = state.probe_owner {
            if summary.token.owner < current {
                return Err(UiEventSendError::StaleProbeOwner {
                    current,
                    received: summary.token.owner,
                });
            }
        }
        if let Some(current) = state.probe_summaries.get(&summary.token.kind) {
            if current.token.owner == summary.token.owner {
                validate_probe_transition(current, summary)?;
            }
        }
    }
    if let UiEvent::CloudUploadProgress { account, .. }
    | UiEvent::CloudUploadRemoved { account, .. } = event
    {
        if state.cloud_account.as_ref() != Some(account) {
            return Err(UiEventSendError::AccountChanged);
        }
    }
    if let UiEvent::CloudAccountChanged {
        generation,
        account,
    } = event
    {
        if account
            .as_ref()
            .is_some_and(|owner| owner.account_generation() != *generation)
        {
            return Err(UiEventSendError::InvalidAccountGeneration);
        }
        if *generation < state.cloud_account_generation
            || (*generation == state.cloud_account_generation && &state.cloud_account != account)
        {
            return Err(UiEventSendError::StaleAccount {
                current: state.cloud_account_generation,
                received: *generation,
            });
        }
    }
    let Some((domain, received)) = generation_domain(event) else {
        return Ok(());
    };
    if let Some(current) = state.generations.get(&domain).copied() {
        if received < current {
            return Err(UiEventSendError::Stale { current, received });
        }
    }
    if domain == GenerationDomain::Microphone {
        if let Some(current) = state
            .microphone_terminal
            .filter(|current| received <= *current)
        {
            return Err(UiEventSendError::Stale { current, received });
        }
        if let Some(current) = state.microphone_error {
            let allowed_stop =
                received == current && matches!(event, UiEvent::MicTestStopped { .. });
            if received < current || (received == current && !allowed_stop) {
                return Err(UiEventSendError::Stale { current, received });
            }
        }
    }
    Ok(())
}

fn record_generation(state: &mut ChannelState, event: &UiEvent) {
    if let UiEvent::WindowLifecycle { snapshot } = event {
        state.lifecycle = Some((snapshot.revision, snapshot.mode));
    }
    if let UiEvent::CatalogSummaryChanged { summary } = event {
        state.catalog = Some(*summary);
    }
    if let UiEvent::SettingsProbeChanged { summary } = event {
        if state.probe_owner != Some(summary.token.owner) {
            state.probe_summaries.clear();
            state.probe_owner = Some(summary.token.owner);
        }
        state
            .probe_summaries
            .insert(summary.token.kind, summary.clone());
    }
    if let Some((domain, generation)) = generation_domain(event) {
        state.generations.insert(domain, generation);
    }
    if let UiEvent::CloudAccountChanged {
        generation,
        account,
    } = event
    {
        state.cloud_account_generation = *generation;
        state.cloud_account.clone_from(account);
        state
            .generations
            .retain(|domain, _| !matches!(domain, GenerationDomain::CloudUpload(_, _)));
    }
    match event {
        UiEvent::MicTestError { generation, .. } => state.microphone_error = Some(*generation),
        UiEvent::MicTestStopped { generation } => state.microphone_terminal = Some(*generation),
        _ => {}
    }
}

fn coalescing_key(event: &UiEvent) -> Option<CoalescingKey> {
    match event {
        UiEvent::Recorder {
            event: RecorderEvent::Status { .. },
            ..
        } => Some(CoalescingKey::RecorderStatus),
        UiEvent::Recorder {
            event: RecorderEvent::MediaRootResolved { .. },
            ..
        } => Some(CoalescingKey::MediaRoot),
        UiEvent::WindowLifecycle { .. } => Some(CoalescingKey::WindowLifecycle),
        UiEvent::MicMonitor { .. } => Some(CoalescingKey::MicrophoneMonitor),
        UiEvent::GameDetection { .. } => Some(CoalescingKey::GameDetection),
        UiEvent::CloudUploadProgress {
            account,
            update: CloudUploadUpdateKind::Bytes,
            progress,
            ..
        } => Some(CoalescingKey::CloudUpload(
            account.clone(),
            progress.local_clip_id.clone(),
        )),
        UiEvent::EnrichmentUpdated { .. } => Some(CoalescingKey::Enrichment),
        UiEvent::CatalogSummaryChanged { .. } => Some(CoalescingKey::CatalogSummary),
        UiEvent::Recorder {
            event: RecorderEvent::Saved { .. } | RecorderEvent::Error { .. },
            ..
        }
        | UiEvent::MicTestError { .. }
        | UiEvent::MicTestStopped { .. }
        | UiEvent::CloudAccountChanged { .. }
        | UiEvent::CloudUploadProgress {
            update: CloudUploadUpdateKind::State,
            ..
        }
        | UiEvent::CloudUploadRemoved { .. }
        | UiEvent::SettingsProbeChanged { .. }
        | UiEvent::UserError { .. } => None,
    }
}

fn is_durable_barrier(event: &UiEvent) -> bool {
    matches!(
        event,
        UiEvent::Recorder {
            event: RecorderEvent::Saved { .. } | RecorderEvent::Error { .. },
            ..
        } | UiEvent::MicTestError { .. }
            | UiEvent::MicTestStopped { .. }
            | UiEvent::CloudAccountChanged { .. }
            | UiEvent::CloudUploadProgress {
                update: CloudUploadUpdateKind::State,
                ..
            }
            | UiEvent::CloudUploadRemoved { .. }
            | UiEvent::SettingsProbeChanged { .. }
            | UiEvent::UserError { .. }
    )
}

fn validate_probe_transition(
    current: &ProbeSummary,
    received: &ProbeSummary,
) -> Result<(), UiEventSendError> {
    if received.token.owner < current.token.owner
        || (received.token.owner == current.token.owner
            && received.token.request_generation < current.token.request_generation)
    {
        return Err(UiEventSendError::StaleProbe {
            current: current.token,
            received: received.token,
        });
    }
    if received.token == current.token && received != current {
        return Err(UiEventSendError::InvalidProbeSummary);
    }
    Ok(())
}

fn queue_limit(event: &UiEvent) -> usize {
    match event {
        UiEvent::MicTestError { .. } => UI_EVENT_CAPACITY + 1,
        UiEvent::MicTestStopped { .. } => UI_EVENT_MAX_BUFFERED,
        _ => UI_EVENT_CAPACITY,
    }
}
