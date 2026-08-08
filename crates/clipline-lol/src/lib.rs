mod client;
mod lcu;
mod normalize;
mod poller;
mod queue;
mod raw;
mod tracker;

pub use client::{Error, LiveClient};
pub use lcu::{league_lockfile_path, LcuClient, LcuError};
pub use normalize::normalize;
pub use poller::{poll_once, poll_once_with_continuity, PollBatch};
pub use queue::{LeagueQueue, LeagueQueueCategory};
pub use raw::{EventData, PlayerListEntry, PlayerScores, RawEvent};
pub use tracker::EventTracker;
