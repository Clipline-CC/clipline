use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LeagueQueueCategory {
    RankedSoloDuo,
    RankedFlex,
    Normal,
    Aram,
    Arena,
    Custom,
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
            _ => (LeagueQueueCategory::Other, "Other"),
        };
        Self {
            id,
            category,
            label: label.to_string(),
        }
    }
}
