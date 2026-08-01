//! Bounded native playback primitives for Clipline-authored media.

mod command;
mod state;

pub use clipline_mp4::PlaybackTime;
pub use command::{
    CommandInbox, EnqueueError, EnqueueOutcome, PlaybackCommand, COMMAND_INBOX_CAPACITY,
};
pub use state::{
    CommandError, PlaybackEvent, PlaybackPhase, PlaybackSnapshot, PlaybackState, WorkGeneration,
};
