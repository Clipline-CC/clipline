use std::path::Path;

use serde::{Deserialize, Serialize};

/// Sentinel queue id for League *client replay* sessions. Not a Riot queue.
pub const REPLAY_QUEUE_ID: u32 = u32::MAX;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LeagueQueueCategory {
    RankedSoloDuo,
    RankedFlex,
    Normal,
    Aram,
    Arena,
    Custom,
    Replay,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeagueQueue {
    pub id: u32,
    pub category: LeagueQueueCategory,
    pub label: String,
}

impl LeagueQueue {
    pub fn from_id(id: u32) -> Self {
        let (category, label) = match id {
            0 => (LeagueQueueCategory::Custom, "Custom"),
            400 => (LeagueQueueCategory::Normal, "Normal Draft"),
            420 => (LeagueQueueCategory::RankedSoloDuo, "Ranked Solo/Duo"),
            430 => (LeagueQueueCategory::Normal, "Normal Blind"),
            440 => (LeagueQueueCategory::RankedFlex, "Ranked Flex"),
            450 => (LeagueQueueCategory::Aram, "ARAM"),
            480 => (LeagueQueueCategory::Normal, "Swiftplay"),
            490 => (LeagueQueueCategory::Normal, "Quickplay"),
            700 => (LeagueQueueCategory::Other, "Clash"),
            720 => (LeagueQueueCategory::Other, "ARAM Clash"),
            900 => (LeagueQueueCategory::Other, "ARURF"),
            1020 => (LeagueQueueCategory::Other, "One for All"),
            1300 => (LeagueQueueCategory::Other, "Nexus Blitz"),
            1400 => (LeagueQueueCategory::Other, "Ultimate Spellbook"),
            1700 | 1710 => (LeagueQueueCategory::Arena, "Arena"),
            1900 => (LeagueQueueCategory::Other, "Pick URF"),
            2300 => (LeagueQueueCategory::Other, "Brawl"),
            2400 => (LeagueQueueCategory::Aram, "ARAM: Mayhem"),
            REPLAY_QUEUE_ID => (LeagueQueueCategory::Replay, "Replay"),
            _ => (LeagueQueueCategory::Other, "Other"),
        };
        Self {
            id,
            category,
            label: label.to_string(),
        }
    }

    /// Tag for watching a `.rofl` in the League client, not a live queue.
    pub fn replay() -> Self {
        Self::from_id(REPLAY_QUEUE_ID)
    }
}

/// True when a League of Legends.exe command line is launching a client replay
/// (an argument whose path ends in `.rofl`). Live matches and spectator
/// sessions do not carry that extension.
pub fn is_league_replay_command_line(command_line: &str) -> bool {
    command_line
        .split(|c: char| c.is_whitespace() || matches!(c, '"' | '\''))
        .any(|token| {
            Path::new(token)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("rofl"))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_constructor_uses_the_reserved_id_and_category() {
        let queue = LeagueQueue::replay();
        assert_eq!(queue.id, REPLAY_QUEUE_ID);
        assert_eq!(queue.category, LeagueQueueCategory::Replay);
        assert_eq!(queue.label, "Replay");
        assert_eq!(LeagueQueue::from_id(REPLAY_QUEUE_ID), queue);
    }

    #[test]
    fn replay_command_line_detects_rofl_arguments() {
        assert!(is_league_replay_command_line(
            r#""C:\Riot Games\League of Legends\Game\League of Legends.exe" "C:\Users\dain\Documents\League of Legends\Replays\NA1-123.rofl""#
        ));
        assert!(is_league_replay_command_line(
            r"League of Legends.exe D:\Replays\foo.ROFL"
        ));
        assert!(!is_league_replay_command_line(
            r#""C:\Riot Games\League of Legends\Game\League of Legends.exe""#
        ));
        assert!(!is_league_replay_command_line(
            r#"League of Legends.exe "spectator 127.0.0.1:8080 key 123 NA1""#
        ));
        assert!(!is_league_replay_command_line("rofl replay wow"));
        assert!(!is_league_replay_command_line(r"C:\clips\game.rofl.bak"));
    }
}
