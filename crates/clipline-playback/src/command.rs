use std::collections::VecDeque;
use std::path::PathBuf;

use clipline_mp4::PlaybackTime;
use thiserror::Error;

use crate::MonotonicTime100ns;

pub const COMMAND_INBOX_CAPACITY: usize = 64;

#[derive(Debug, Clone, PartialEq)]
pub enum PlaybackCommand {
    Open { path: PathBuf },
    Play,
    Pause,
    Seek { position: PlaybackTime },
    Step { frames: i32 },
    SetTracks { audio_track_indices: Vec<usize> },
    SetVolume { volume: f32 },
    SetRate { rate: f32 },
    Close,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnqueueOutcome {
    Queued,
    Replaced,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum EnqueueError {
    #[error("playback command queue is full (capacity {capacity})")]
    QueueFull { capacity: usize },
}

#[derive(Debug, Clone, PartialEq)]
pub struct AcceptedCommand {
    pub command: PlaybackCommand,
    pub accepted_at: MonotonicTime100ns,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CoalescingKey {
    Transport,
    Seek,
    Tracks,
    Volume,
    Rate,
}

impl PlaybackCommand {
    fn is_resource_fence(&self) -> bool {
        matches!(self, Self::Open { .. } | Self::Close)
    }

    fn coalescing_key(&self) -> Option<CoalescingKey> {
        match self {
            Self::Play | Self::Pause => Some(CoalescingKey::Transport),
            Self::Seek { .. } => Some(CoalescingKey::Seek),
            Self::SetTracks { .. } => Some(CoalescingKey::Tracks),
            Self::SetVolume { .. } => Some(CoalescingKey::Volume),
            Self::SetRate { .. } => Some(CoalescingKey::Rate),
            Self::Open { .. } | Self::Step { .. } | Self::Close => None,
        }
    }
}

#[derive(Debug, Default)]
pub struct CommandInbox {
    pending: VecDeque<AcceptedCommand>,
}

impl CommandInbox {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.pending.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    pub fn enqueue(&mut self, command: PlaybackCommand) -> Result<EnqueueOutcome, EnqueueError> {
        self.enqueue_at(command, MonotonicTime100ns::new(0))
    }

    pub fn enqueue_at(
        &mut self,
        command: PlaybackCommand,
        accepted_at: MonotonicTime100ns,
    ) -> Result<EnqueueOutcome, EnqueueError> {
        if matches!(command, PlaybackCommand::Close)
            && self
                .pending
                .back()
                .is_some_and(|item| matches!(item.command, PlaybackCommand::Close))
        {
            if let Some(item) = self.pending.back_mut() {
                item.accepted_at = accepted_at;
            }
            return Ok(EnqueueOutcome::Replaced);
        }

        if let Some(key) = command.coalescing_key() {
            let replace_index = self
                .pending
                .iter()
                .enumerate()
                .rev()
                .take_while(|(_, item)| !item.command.is_resource_fence())
                .find_map(|(index, item)| {
                    (item.command.coalescing_key() == Some(key)).then_some(index)
                });
            if let Some(index) = replace_index {
                self.pending.remove(index);
                self.pending.push_back(AcceptedCommand {
                    command,
                    accepted_at,
                });
                return Ok(EnqueueOutcome::Replaced);
            }
        }

        let limit = if matches!(command, PlaybackCommand::Close) {
            COMMAND_INBOX_CAPACITY
        } else {
            COMMAND_INBOX_CAPACITY - 1
        };
        if self.pending.len() >= limit {
            return Err(EnqueueError::QueueFull {
                capacity: COMMAND_INBOX_CAPACITY,
            });
        }
        self.pending.push_back(AcceptedCommand {
            command,
            accepted_at,
        });
        Ok(EnqueueOutcome::Queued)
    }

    pub fn pop_front(&mut self) -> Option<PlaybackCommand> {
        self.pop_front_accepted().map(|accepted| accepted.command)
    }

    pub fn pop_front_accepted(&mut self) -> Option<AcceptedCommand> {
        self.pending.pop_front()
    }
}
