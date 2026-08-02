use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use thiserror::Error;

use crate::{
    AcceptedCommand, CommandInbox, EnqueueError, EnqueueOutcome, MonotonicTime100ns, PipelineToken,
    PlaybackCommand, PlaybackEvent, PlaybackMetrics, PlaybackSnapshot,
};

pub const SESSION_UPDATE_CAPACITY: usize = 64;
pub const SESSION_MAX_WAIT: Duration = Duration::from_millis(10);

#[derive(Debug, Clone, PartialEq)]
pub enum SessionUpdatePayload {
    Snapshot(PlaybackSnapshot),
    Event(PlaybackEvent),
    Metrics(Box<PlaybackMetrics>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionUpdate {
    pub sequence: u64,
    pub token: PipelineToken,
    pub payload: SessionUpdatePayload,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdatePublishOutcome {
    Queued,
    Replaced,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum SessionSendError {
    #[error("playback session command queue is full (capacity {capacity})")]
    Full { capacity: usize },
    #[error("playback session runtime is disconnected")]
    Disconnected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum SessionUpdateError {
    #[error("playback session update queue is full (capacity {capacity})")]
    Full { capacity: usize },
    #[error("playback session client is disconnected")]
    Disconnected,
    #[error("playback session update sequence is exhausted")]
    SequenceExhausted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UpdateKey {
    Snapshot,
    Metrics,
}

impl SessionUpdatePayload {
    const fn coalescing_key(&self) -> Option<UpdateKey> {
        match self {
            Self::Snapshot(_) => Some(UpdateKey::Snapshot),
            Self::Metrics(_) => Some(UpdateKey::Metrics),
            Self::Event(_) => None,
        }
    }

    const fn is_terminal_event(&self) -> bool {
        matches!(
            self,
            Self::Event(
                PlaybackEvent::Ended { .. }
                    | PlaybackEvent::Error { .. }
                    | PlaybackEvent::Closed { .. }
            )
        )
    }
}

#[derive(Debug)]
struct SessionPorts {
    commands: CommandInbox,
    updates: VecDeque<SessionUpdate>,
    next_update_sequence: u64,
    client_connected: bool,
    runtime_connected: bool,
}

impl Default for SessionPorts {
    fn default() -> Self {
        Self {
            commands: CommandInbox::new(),
            updates: VecDeque::new(),
            next_update_sequence: 0,
            client_connected: true,
            runtime_connected: true,
        }
    }
}

#[derive(Debug)]
struct SessionShared {
    ports: Mutex<SessionPorts>,
    command_ready: Condvar,
    anchor: Instant,
}

#[derive(Debug)]
pub struct SessionClient {
    shared: Arc<SessionShared>,
}

#[derive(Debug)]
pub struct SessionRuntime {
    shared: Arc<SessionShared>,
}

pub fn session_channel() -> (SessionClient, SessionRuntime) {
    let shared = Arc::new(SessionShared {
        ports: Mutex::new(SessionPorts::default()),
        command_ready: Condvar::new(),
        anchor: Instant::now(),
    });
    (
        SessionClient {
            shared: Arc::clone(&shared),
        },
        SessionRuntime { shared },
    )
}

impl SessionClient {
    pub fn try_send(&self, command: PlaybackCommand) -> Result<EnqueueOutcome, SessionSendError> {
        let elapsed_100ns = self.shared.anchor.elapsed().as_nanos() / 100;
        let accepted_at = MonotonicTime100ns::new(u64::try_from(elapsed_100ns).unwrap_or(u64::MAX));
        self.try_send_at(command, accepted_at)
    }

    pub fn try_send_at(
        &self,
        command: PlaybackCommand,
        accepted_at: MonotonicTime100ns,
    ) -> Result<EnqueueOutcome, SessionSendError> {
        let mut ports = self
            .shared
            .ports
            .lock()
            .map_err(|_| SessionSendError::Disconnected)?;
        if !ports.runtime_connected {
            return Err(SessionSendError::Disconnected);
        }
        let outcome =
            ports
                .commands
                .enqueue_at(command, accepted_at)
                .map_err(|error| match error {
                    EnqueueError::QueueFull { capacity } => SessionSendError::Full { capacity },
                })?;
        drop(ports);
        self.shared.command_ready.notify_one();
        Ok(outcome)
    }

    pub fn try_recv_update(&self) -> Option<SessionUpdate> {
        self.shared
            .ports
            .lock()
            .ok()
            .and_then(|mut ports| ports.updates.pop_front())
    }
}

impl Drop for SessionClient {
    fn drop(&mut self) {
        if let Ok(mut ports) = self.shared.ports.lock() {
            ports.client_connected = false;
        }
        self.shared.command_ready.notify_all();
    }
}

impl SessionRuntime {
    pub fn try_recv_command(&mut self) -> Option<AcceptedCommand> {
        self.shared
            .ports
            .lock()
            .ok()
            .and_then(|mut ports| ports.commands.pop_front_accepted())
    }

    pub fn wait_for_command(&mut self, timeout: Duration) -> Option<AcceptedCommand> {
        let mut ports = self.shared.ports.lock().ok()?;
        if ports.commands.is_empty() && ports.client_connected {
            let waited = self
                .shared
                .command_ready
                .wait_timeout(ports, timeout.min(SESSION_MAX_WAIT))
                .ok()?;
            ports = waited.0;
        }
        ports.commands.pop_front_accepted()
    }

    pub fn publish_update(
        &mut self,
        token: PipelineToken,
        payload: SessionUpdatePayload,
    ) -> Result<UpdatePublishOutcome, SessionUpdateError> {
        let mut ports = self
            .shared
            .ports
            .lock()
            .map_err(|_| SessionUpdateError::Disconnected)?;
        if !ports.client_connected {
            return Err(SessionUpdateError::Disconnected);
        }
        let replacement = payload.coalescing_key().and_then(|key| {
            ports
                .updates
                .iter()
                .enumerate()
                .rev()
                .take_while(|(_, update)| !matches!(update.payload, SessionUpdatePayload::Event(_)))
                .find_map(|(index, update)| {
                    (update.payload.coalescing_key() == Some(key)).then_some(index)
                })
        });
        let limit = if payload.is_terminal_event() {
            SESSION_UPDATE_CAPACITY
        } else {
            SESSION_UPDATE_CAPACITY - 1
        };
        if replacement.is_none() && ports.updates.len() >= limit {
            return Err(SessionUpdateError::Full {
                capacity: SESSION_UPDATE_CAPACITY,
            });
        }
        let sequence = ports
            .next_update_sequence
            .checked_add(1)
            .ok_or(SessionUpdateError::SequenceExhausted)?;
        ports.next_update_sequence = sequence;
        let update = SessionUpdate {
            sequence,
            token,
            payload,
        };
        if let Some(index) = replacement {
            ports.updates[index] = update;
            Ok(UpdatePublishOutcome::Replaced)
        } else {
            ports.updates.push_back(update);
            Ok(UpdatePublishOutcome::Queued)
        }
    }
}

impl Drop for SessionRuntime {
    fn drop(&mut self) {
        if let Ok(mut ports) = self.shared.ports.lock() {
            ports.runtime_connected = false;
        }
        self.shared.command_ready.notify_all();
    }
}
