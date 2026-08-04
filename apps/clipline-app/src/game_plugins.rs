//! Tauri marker-source adapter over the shared game-plugin registry.

use std::sync::mpsc::Receiver;
use std::time::Instant;

use crate::markers::PollerMsg;

pub use clipline_games::plugin::*;

#[derive(Clone, Debug)]
pub struct GameEventSourceContext {
    pub lol_url: Option<String>,
    pub recording_t0: Instant,
}

pub fn spawn_event_source(
    profile_id: Option<&str>,
    context: GameEventSourceContext,
) -> Option<Receiver<PollerMsg>> {
    match event_source_for_profile(profile_id)? {
        LEAGUE_LIVE_CLIENT_EVENT_SOURCE => {
            Some(crate::markers::spawn(context.lol_url, context.recording_t0))
        }
        _ => None,
    }
}
