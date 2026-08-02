use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use thiserror::Error;

use crate::{ShellCommand, ShellSequence};

pub const SHELL_COMMAND_CAPACITY: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SequencedShellCommand {
    pub sequence: ShellSequence,
    pub command: ShellCommand,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellCommandPublishOutcome {
    Queued,
    Replaced,
    AlreadyQueued,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ShellCommandSendError {
    #[error("shell command consumer is disconnected")]
    Disconnected,
    #[error("shell command queue is full at capacity {capacity}")]
    Full { capacity: usize },
    #[error("shell command sequence is exhausted")]
    SequenceExhausted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ShellCommandReceiveError {
    #[error("all shell command producers are disconnected")]
    Disconnected,
}

struct ChannelState {
    queue: VecDeque<SequencedShellCommand>,
    next_sequence: ShellSequence,
    receiver_connected: bool,
    sender_count: usize,
}

struct Shared {
    state: Mutex<ChannelState>,
    ready: Condvar,
}

pub struct ShellCommandSender {
    shared: Arc<Shared>,
}

pub struct ShellCommandReceiver {
    shared: Arc<Shared>,
}

#[must_use]
pub fn shell_command_channel() -> (ShellCommandSender, ShellCommandReceiver) {
    shell_command_channel_starting_at(ShellSequence::INITIAL)
}

/// Constructs a command port with an explicit sequence cursor.
///
/// This is public so exhaustion behavior can be proven without billions of
/// commands. Production callers should use [`shell_command_channel`].
#[doc(hidden)]
#[must_use]
pub fn shell_command_channel_starting_at(
    next_sequence: ShellSequence,
) -> (ShellCommandSender, ShellCommandReceiver) {
    let shared = Arc::new(Shared {
        state: Mutex::new(ChannelState {
            queue: VecDeque::with_capacity(SHELL_COMMAND_CAPACITY),
            next_sequence,
            receiver_connected: true,
            sender_count: 1,
        }),
        ready: Condvar::new(),
    });
    (
        ShellCommandSender {
            shared: Arc::clone(&shared),
        },
        ShellCommandReceiver { shared },
    )
}

impl Clone for ShellCommandSender {
    fn clone(&self) -> Self {
        if let Ok(mut state) = self.shared.state.lock() {
            state.sender_count = state.sender_count.saturating_add(1);
        }
        Self {
            shared: Arc::clone(&self.shared),
        }
    }
}

impl ShellCommandSender {
    pub fn try_send(
        &self,
        command: ShellCommand,
    ) -> Result<ShellCommandPublishOutcome, ShellCommandSendError> {
        let mut state = self
            .shared
            .state
            .lock()
            .map_err(|_| ShellCommandSendError::Disconnected)?;
        if !state.receiver_connected {
            return Err(ShellCommandSendError::Disconnected);
        }
        if command == ShellCommand::Quit
            && state
                .queue
                .iter()
                .any(|queued| queued.command == ShellCommand::Quit)
        {
            return Ok(ShellCommandPublishOutcome::AlreadyQueued);
        }

        let replacement = command.is_coalescable().then(|| {
            state
                .queue
                .iter()
                .enumerate()
                .rev()
                .take_while(|(_, queued)| !queued.command.is_barrier())
                .find_map(|(index, queued)| (queued.command == command).then_some(index))
        });
        let replacement = replacement.flatten();
        let limit = if command == ShellCommand::Quit {
            SHELL_COMMAND_CAPACITY
        } else {
            SHELL_COMMAND_CAPACITY - 1
        };
        if replacement.is_none() && state.queue.len() >= limit {
            return Err(ShellCommandSendError::Full {
                capacity: SHELL_COMMAND_CAPACITY,
            });
        }
        let sequence = state
            .next_sequence
            .checked_next()
            .map_err(|_| ShellCommandSendError::SequenceExhausted)?;
        state.next_sequence = sequence;
        let update = SequencedShellCommand { sequence, command };
        let outcome = if let Some(index) = replacement {
            state.queue.remove(index);
            state.queue.push_back(update);
            ShellCommandPublishOutcome::Replaced
        } else {
            state.queue.push_back(update);
            ShellCommandPublishOutcome::Queued
        };
        drop(state);
        self.shared.ready.notify_one();
        Ok(outcome)
    }
}

impl Drop for ShellCommandSender {
    fn drop(&mut self) {
        if let Ok(mut state) = self.shared.state.lock() {
            state.sender_count = state.sender_count.saturating_sub(1);
        }
        self.shared.ready.notify_all();
    }
}

impl ShellCommandReceiver {
    #[must_use]
    pub fn try_recv(&self) -> Option<SequencedShellCommand> {
        self.shared
            .state
            .lock()
            .ok()
            .and_then(|mut state| state.queue.pop_front())
    }

    pub fn wait_recv(
        &self,
        timeout: Duration,
    ) -> Result<Option<SequencedShellCommand>, ShellCommandReceiveError> {
        let mut state = self
            .shared
            .state
            .lock()
            .map_err(|_| ShellCommandReceiveError::Disconnected)?;
        if state.queue.is_empty() && state.sender_count != 0 {
            state = self
                .shared
                .ready
                .wait_timeout(state, timeout)
                .map_err(|_| ShellCommandReceiveError::Disconnected)?
                .0;
        }
        if let Some(update) = state.queue.pop_front() {
            Ok(Some(update))
        } else if state.sender_count == 0 {
            Err(ShellCommandReceiveError::Disconnected)
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

impl Drop for ShellCommandReceiver {
    fn drop(&mut self) {
        if let Ok(mut state) = self.shared.state.lock() {
            state.receiver_connected = false;
            state.queue.clear();
        }
        self.shared.ready.notify_all();
    }
}
