use std::collections::VecDeque;
use std::path::PathBuf;

use clipline_mp4::PlaybackTime;
use thiserror::Error;

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
    pending: VecDeque<PlaybackCommand>,
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
        if matches!(command, PlaybackCommand::Close)
            && self
                .pending
                .back()
                .is_some_and(|item| matches!(item, PlaybackCommand::Close))
        {
            return Ok(EnqueueOutcome::Replaced);
        }

        if let Some(key) = command.coalescing_key() {
            let replace_index = self
                .pending
                .iter()
                .enumerate()
                .rev()
                .take_while(|(_, item)| !item.is_resource_fence())
                .find_map(|(index, item)| (item.coalescing_key() == Some(key)).then_some(index));
            if let Some(index) = replace_index {
                self.pending.remove(index);
                self.pending.push_back(command);
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
        self.pending.push_back(command);
        Ok(EnqueueOutcome::Queued)
    }

    pub fn pop_front(&mut self) -> Option<PlaybackCommand> {
        self.pending.pop_front()
    }
}
