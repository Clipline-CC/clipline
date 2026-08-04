//! Bounded, token-fenced execution for Settings discovery probes.
//!
//! The executor owns scheduling, coalescing, cancellation checkpoints, result
//! delivery, and worker joins. Concrete display/audio/encoder/game payloads
//! stay in their domain crates and implement [`BoundedProbePayload`].

use std::cell::Cell;
use std::collections::VecDeque;
use std::fmt;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{SettingsSessionGeneration, SettingsTab};

pub const PROBE_WORKER_COUNT: usize = 2;
pub const MAX_PROBE_ERROR_BYTES: usize = 64 * 1024;
pub const MAX_PROBE_WORK_BYTES: usize = 64 * 1024;
pub const PROBE_RESULT_CAPACITY: usize = ProbeKind::COUNT;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum ProbeKind {
    Displays,
    AudioEndpoints,
    Encoders,
    GameWindows,
    InstalledGames,
    GamePlugins,
    Storage,
    PlaybackCapabilities,
}

impl ProbeKind {
    pub const ALL: [Self; 8] = [
        Self::Displays,
        Self::AudioEndpoints,
        Self::Encoders,
        Self::GameWindows,
        Self::InstalledGames,
        Self::GamePlugins,
        Self::Storage,
        Self::PlaybackCapabilities,
    ];
    pub const COUNT: usize = Self::ALL.len();

    pub const fn settings_tab(self) -> SettingsTab {
        match self {
            Self::Displays | Self::AudioEndpoints => SettingsTab::Capture,
            Self::Encoders | Self::PlaybackCapabilities => SettingsTab::Recording,
            Self::GameWindows | Self::InstalledGames | Self::GamePlugins => SettingsTab::Games,
            Self::Storage => SettingsTab::Storage,
        }
    }

    const fn index(self) -> usize {
        self as usize
    }
}

macro_rules! generation_type {
    ($name:ident) => {
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(u64);

        impl $name {
            pub const fn new(value: u64) -> Self {
                Self(value)
            }

            pub const fn get(self) -> u64 {
                self.0
            }
        }
    };
}

generation_type!(SettingsAttachmentGeneration);
generation_type!(SettingsForegroundGeneration);
generation_type!(ProbeRequestGeneration);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ProbeSessionOwner {
    pub settings_session: SettingsSessionGeneration,
    pub attachment: SettingsAttachmentGeneration,
    pub foreground: SettingsForegroundGeneration,
}

impl ProbeSessionOwner {
    pub const fn new(
        settings_session: SettingsSessionGeneration,
        attachment: SettingsAttachmentGeneration,
        foreground: SettingsForegroundGeneration,
    ) -> Self {
        Self {
            settings_session,
            attachment,
            foreground,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ProbeToken {
    pub owner: ProbeSessionOwner,
    pub kind: ProbeKind,
    pub request_generation: ProbeRequestGeneration,
}

impl ProbeToken {
    const fn key(self) -> ProbeCoalesceKey {
        ProbeCoalesceKey {
            owner: self.owner,
            kind: self.kind,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ProbeCoalesceKey {
    owner: ProbeSessionOwner,
    kind: ProbeKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbePhase {
    Pending,
    Ready,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeSummary {
    pub token: ProbeToken,
    pub phase: ProbePhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ProbeSummary {
    pub fn validate(&self) -> Result<(), String> {
        match (&self.phase, &self.error) {
            (ProbePhase::Failed, Some(error)) if !error.is_empty() => {
                if error.len() > MAX_PROBE_ERROR_BYTES {
                    return Err(format!(
                        "probe error is {} bytes; maximum is {MAX_PROBE_ERROR_BYTES}",
                        error.len()
                    ));
                }
                Ok(())
            }
            (ProbePhase::Failed, _) => Err("failed probe summary requires an error".into()),
            (_, None) => Ok(()),
            (_, Some(_)) => Err("non-failed probe summary cannot carry an error".into()),
        }
    }
}

pub trait BoundedProbePayload: Send + 'static {
    fn validate_bounds(&self) -> Result<(), String>;
}

#[derive(Debug, PartialEq, Eq)]
pub enum ProbeOutcome<P> {
    Ready(P),
    Failed(String),
}

#[derive(Debug, PartialEq, Eq)]
pub struct ProbeResult<P> {
    pub token: ProbeToken,
    pub outcome: ProbeOutcome<P>,
}

impl<P> ProbeResult<P> {
    pub fn summary(&self) -> ProbeSummary {
        let (phase, error) = match &self.outcome {
            ProbeOutcome::Ready(_) => (ProbePhase::Ready, None),
            ProbeOutcome::Failed(error) => (ProbePhase::Failed, Some(error.clone())),
        };
        ProbeSummary {
            token: self.token,
            phase,
            error,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ProbeAdmissionError {
    #[error("settings probe session is disconnected")]
    Disconnected,
    #[error("probe {kind:?} does not belong to active tab {active_tab:?}")]
    InactiveTab {
        kind: ProbeKind,
        active_tab: SettingsTab,
    },
    #[error("probe request generation is exhausted for {kind:?}")]
    GenerationExhausted { kind: ProbeKind },
}

struct ProbeFenceState {
    owner: ProbeSessionOwner,
    active_tab: SettingsTab,
    request_generations: [u64; ProbeKind::COUNT],
    current: [Option<ProbeToken>; ProbeKind::COUNT],
    connected: bool,
}

#[derive(Clone)]
pub struct ProbeSessionFence {
    state: Arc<Mutex<ProbeFenceState>>,
}

impl fmt::Debug for ProbeSessionFence {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProbeSessionFence")
            .finish_non_exhaustive()
    }
}

impl ProbeSessionFence {
    pub fn new(owner: ProbeSessionOwner, active_tab: SettingsTab) -> Self {
        Self::open_with_request_generation(owner, active_tab, 0)
    }

    pub fn open_with_request_generation(
        owner: ProbeSessionOwner,
        active_tab: SettingsTab,
        request_generation: u64,
    ) -> Self {
        Self {
            state: Arc::new(Mutex::new(ProbeFenceState {
                owner,
                active_tab,
                request_generations: [request_generation; ProbeKind::COUNT],
                current: [None; ProbeKind::COUNT],
                connected: true,
            })),
        }
    }

    pub fn request(&self, kind: ProbeKind) -> Result<ProbeToken, ProbeAdmissionError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !state.connected {
            return Err(ProbeAdmissionError::Disconnected);
        }
        if kind.settings_tab() != state.active_tab {
            return Err(ProbeAdmissionError::InactiveTab {
                kind,
                active_tab: state.active_tab,
            });
        }
        let index = kind.index();
        let next = state.request_generations[index]
            .checked_add(1)
            .ok_or(ProbeAdmissionError::GenerationExhausted { kind })?;
        let token = ProbeToken {
            owner: state.owner,
            kind,
            request_generation: ProbeRequestGeneration::new(next),
        };
        state.request_generations[index] = next;
        state.current[index] = Some(token);
        Ok(token)
    }

    pub fn set_active_tab(&self, active_tab: SettingsTab) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.active_tab = active_tab;
        state.current.fill(None);
    }

    pub fn replace_owner(&self, owner: ProbeSessionOwner, active_tab: SettingsTab) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.owner = owner;
        state.active_tab = active_tab;
        state.request_generations.fill(0);
        state.current.fill(None);
        state.connected = true;
    }

    pub fn disconnect(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.connected = false;
        state.current.fill(None);
    }
}

pub trait ProbeTokenFence: Send + Sync + 'static {
    fn is_current(&self, token: ProbeToken) -> bool;
}

impl ProbeTokenFence for ProbeSessionFence {
    fn is_current(&self, token: ProbeToken) -> bool {
        self.state.lock().is_ok_and(|state| {
            state.connected
                && state.owner == token.owner
                && state.active_tab == token.kind.settings_tab()
                && state.current[token.kind.index()] == Some(token)
        })
    }
}

pub struct ProbeExecutionContext {
    token: ProbeToken,
    fence: Arc<dyn ProbeTokenFence>,
    activation_checked: Cell<bool>,
}

impl ProbeExecutionContext {
    pub const fn token(&self) -> ProbeToken {
        self.token
    }

    /// Must be called immediately after COM/OS/device activation and before
    /// enumeration or allocation proceeds.
    pub fn checkpoint_after_activation(&self) -> Result<(), String> {
        self.activation_checked.set(true);
        if self.fence.is_current(self.token) {
            Ok(())
        } else {
            Err("settings probe became stale after activation".into())
        }
    }
}

type ProbeWork<P> = Box<dyn FnOnce(&ProbeExecutionContext) -> Result<P, String> + Send + 'static>;

struct ProbeJob<P> {
    token: ProbeToken,
    work: ProbeWork<P>,
}

struct ProbeKindSlot<P> {
    active: bool,
    pending: Option<ProbeJob<P>>,
}

struct ProbeExecutorState<P> {
    slots: [ProbeKindSlot<P>; ProbeKind::COUNT],
    shutdown: bool,
}

struct ProbeExecutorShared<P> {
    state: Mutex<ProbeExecutorState<P>>,
    ready: Condvar,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeSubmitOutcome {
    Queued,
    Replaced,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ProbeSubmitError {
    #[error("settings probe executor is disconnected")]
    Disconnected,
    #[error("settings probe lane {kind:?} is full")]
    Full { kind: ProbeKind },
    #[error("settings probe request is stale")]
    Stale,
    #[error("settings probe work owns {actual} bytes; maximum is {maximum}")]
    WorkTooLarge { actual: usize, maximum: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeResultPublishOutcome {
    Queued,
    Replaced,
}

struct ProbeResultState<P> {
    queue: VecDeque<ProbeResult<P>>,
    receiver_connected: bool,
    sender_count: usize,
}

struct ProbeResultShared<P> {
    state: Mutex<ProbeResultState<P>>,
    ready: Condvar,
}

struct ProbeResultSender<P> {
    shared: Arc<ProbeResultShared<P>>,
}

pub struct ProbeResultReceiver<P> {
    shared: Arc<ProbeResultShared<P>>,
}

fn probe_result_channel<P>() -> (ProbeResultSender<P>, ProbeResultReceiver<P>) {
    let shared = Arc::new(ProbeResultShared {
        state: Mutex::new(ProbeResultState {
            queue: VecDeque::new(),
            receiver_connected: true,
            sender_count: 1,
        }),
        ready: Condvar::new(),
    });
    (
        ProbeResultSender {
            shared: shared.clone(),
        },
        ProbeResultReceiver { shared },
    )
}

impl<P> Clone for ProbeResultSender<P> {
    fn clone(&self) -> Self {
        if let Ok(mut state) = self.shared.state.lock() {
            state.sender_count = state.sender_count.saturating_add(1);
        }
        Self {
            shared: self.shared.clone(),
        }
    }
}

impl<P> Drop for ProbeResultSender<P> {
    fn drop(&mut self) {
        if let Ok(mut state) = self.shared.state.lock() {
            state.sender_count = state.sender_count.saturating_sub(1);
        }
        self.shared.ready.notify_all();
    }
}

impl<P> ProbeResultSender<P> {
    fn try_publish(
        &self,
        result: ProbeResult<P>,
    ) -> Result<ProbeResultPublishOutcome, ProbeSubmitError> {
        let mut state = self
            .shared
            .state
            .lock()
            .map_err(|_| ProbeSubmitError::Disconnected)?;
        if !state.receiver_connected {
            return Err(ProbeSubmitError::Disconnected);
        }
        let key = result.token.key();
        if let Some(index) = state
            .queue
            .iter()
            .position(|queued| queued.token.key() == key)
        {
            state.queue.remove(index);
            state.queue.push_back(result);
            drop(state);
            self.shared.ready.notify_one();
            return Ok(ProbeResultPublishOutcome::Replaced);
        }
        if state.queue.len() >= PROBE_RESULT_CAPACITY {
            return Err(ProbeSubmitError::Full {
                kind: result.token.kind,
            });
        }
        state.queue.push_back(result);
        drop(state);
        self.shared.ready.notify_one();
        Ok(ProbeResultPublishOutcome::Queued)
    }
}

impl<P> ProbeResultReceiver<P> {
    pub fn try_recv(&self) -> Option<ProbeResult<P>> {
        self.shared
            .state
            .lock()
            .ok()
            .and_then(|mut state| state.queue.pop_front())
    }

    pub fn wait_recv(&self, timeout: Duration) -> Option<ProbeResult<P>> {
        let mut state = self.shared.state.lock().ok()?;
        if state.queue.is_empty() && state.sender_count != 0 {
            state = self.shared.ready.wait_timeout(state, timeout).ok()?.0;
        }
        state.queue.pop_front()
    }

    pub fn len(&self) -> usize {
        self.shared
            .state
            .lock()
            .map_or(0, |state| state.queue.len())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl<P> Drop for ProbeResultReceiver<P> {
    fn drop(&mut self) {
        if let Ok(mut state) = self.shared.state.lock() {
            state.receiver_connected = false;
            state.queue.clear();
        }
        self.shared.ready.notify_all();
    }
}

pub struct ProbeExecutor<P> {
    shared: Arc<ProbeExecutorShared<P>>,
    fence: Arc<dyn ProbeTokenFence>,
    workers: Vec<JoinHandle<()>>,
}

impl<P> fmt::Debug for ProbeExecutor<P> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProbeExecutor")
            .finish_non_exhaustive()
    }
}

impl<P: BoundedProbePayload> ProbeExecutor<P> {
    pub fn new(fence: Arc<dyn ProbeTokenFence>) -> Result<(Self, ProbeResultReceiver<P>), String> {
        let shared = Arc::new(ProbeExecutorShared {
            state: Mutex::new(ProbeExecutorState {
                slots: std::array::from_fn(|_| ProbeKindSlot {
                    active: false,
                    pending: None,
                }),
                shutdown: false,
            }),
            ready: Condvar::new(),
        });
        let (results, receiver) = probe_result_channel();
        let mut workers = Vec::new();
        for index in 0..PROBE_WORKER_COUNT {
            let worker_shared = shared.clone();
            let worker_fence = fence.clone();
            let worker_results = results.clone();
            match std::thread::Builder::new()
                .name(format!("clipline-settings-probe-{index}"))
                .spawn(move || worker_loop(worker_shared, worker_fence, worker_results))
            {
                Ok(worker) => workers.push(worker),
                Err(error) => {
                    if let Ok(mut state) = shared.state.lock() {
                        state.shutdown = true;
                        for slot in &mut state.slots {
                            slot.pending = None;
                        }
                    }
                    shared.ready.notify_all();
                    for worker in workers {
                        let _ = worker.join();
                    }
                    return Err(format!("spawn settings probe worker: {error}"));
                }
            }
        }
        drop(results);
        Ok((
            Self {
                shared,
                fence,
                workers,
            },
            receiver,
        ))
    }

    pub fn submit(
        &self,
        token: ProbeToken,
        owned_work_bytes: usize,
        work: impl FnOnce(&ProbeExecutionContext) -> Result<P, String> + Send + 'static,
    ) -> Result<ProbeSubmitOutcome, ProbeSubmitError> {
        let work_bytes = std::mem::size_of_val(&work)
            .checked_add(owned_work_bytes)
            .ok_or(ProbeSubmitError::WorkTooLarge {
                actual: usize::MAX,
                maximum: MAX_PROBE_WORK_BYTES,
            })?;
        if work_bytes > MAX_PROBE_WORK_BYTES {
            return Err(ProbeSubmitError::WorkTooLarge {
                actual: work_bytes,
                maximum: MAX_PROBE_WORK_BYTES,
            });
        }
        if !self.fence.is_current(token) {
            return Err(ProbeSubmitError::Stale);
        }
        let mut state = self
            .shared
            .state
            .lock()
            .map_err(|_| ProbeSubmitError::Disconnected)?;
        if state.shutdown {
            return Err(ProbeSubmitError::Disconnected);
        }
        let slot = &mut state.slots[token.kind.index()];
        if slot
            .pending
            .as_ref()
            .is_some_and(|pending| !self.fence.is_current(pending.token))
        {
            slot.pending = None;
        }
        let outcome = match slot.pending.as_ref() {
            None => ProbeSubmitOutcome::Queued,
            Some(pending) if pending.token.key() == token.key() => {
                if pending.token.request_generation >= token.request_generation {
                    return Err(ProbeSubmitError::Stale);
                }
                ProbeSubmitOutcome::Replaced
            }
            Some(_) => return Err(ProbeSubmitError::Full { kind: token.kind }),
        };
        slot.pending = Some(ProbeJob {
            token,
            work: Box::new(work),
        });
        drop(state);
        self.shared.ready.notify_one();
        Ok(outcome)
    }

    pub fn shutdown(&mut self) {
        if let Ok(mut state) = self.shared.state.lock() {
            state.shutdown = true;
            for slot in &mut state.slots {
                slot.pending = None;
            }
        }
        self.shared.ready.notify_all();
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }

    pub fn worker_count(&self) -> usize {
        self.workers.len()
    }
}

impl<P> Drop for ProbeExecutor<P> {
    fn drop(&mut self) {
        if let Ok(mut state) = self.shared.state.lock() {
            state.shutdown = true;
            for slot in &mut state.slots {
                slot.pending = None;
            }
        }
        self.shared.ready.notify_all();
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

fn worker_loop<P: BoundedProbePayload>(
    shared: Arc<ProbeExecutorShared<P>>,
    fence: Arc<dyn ProbeTokenFence>,
    results: ProbeResultSender<P>,
) {
    loop {
        let (kind, job) = {
            let mut state = match shared.state.lock() {
                Ok(state) => state,
                Err(poisoned) => poisoned.into_inner(),
            };
            loop {
                if state.shutdown {
                    return;
                }
                if let Some((index, slot)) = state
                    .slots
                    .iter_mut()
                    .enumerate()
                    .find(|(_, slot)| !slot.active && slot.pending.is_some())
                {
                    slot.active = true;
                    let job = slot.pending.take().expect("pending probe job");
                    break (ProbeKind::ALL[index], job);
                }
                state = match shared.ready.wait(state) {
                    Ok(state) => state,
                    Err(poisoned) => poisoned.into_inner(),
                };
            }
        };

        if fence.is_current(job.token) {
            let context = ProbeExecutionContext {
                token: job.token,
                fence: fence.clone(),
                activation_checked: Cell::new(false),
            };
            let outcome = match catch_unwind(AssertUnwindSafe(|| (job.work)(&context))) {
                Ok(Ok(_payload)) if !context.activation_checked.get() => ProbeOutcome::Failed(
                    "probe completed without the post-activation ownership checkpoint".into(),
                ),
                Ok(Ok(payload)) => match payload.validate_bounds() {
                    Ok(()) => ProbeOutcome::Ready(payload),
                    Err(error) => ProbeOutcome::Failed(bounded_error(error)),
                },
                Ok(Err(error)) => ProbeOutcome::Failed(bounded_error(error)),
                Err(_) => ProbeOutcome::Failed("settings probe worker panicked".into()),
            };
            if fence.is_current(job.token) {
                let _ = results.try_publish(ProbeResult {
                    token: job.token,
                    outcome,
                });
            }
        }

        let mut state = match shared.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.slots[kind.index()].active = false;
        drop(state);
        shared.ready.notify_all();
    }
}

fn bounded_error(mut error: String) -> String {
    if error.is_empty() {
        return "settings probe failed".into();
    }
    if error.len() <= MAX_PROBE_ERROR_BYTES {
        return error;
    }
    let mut boundary = MAX_PROBE_ERROR_BYTES;
    while !error.is_char_boundary(boundary) {
        boundary -= 1;
    }
    error.truncate(boundary);
    error
}
