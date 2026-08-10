//! League of Legends game-type recording gate settings persisted in
//! `settings.json`.
//!
//! When any category is switched off, automatic recording of detected League
//! games consults the local client's queue tag before starting. Manual
//! session recording always bypasses the gate.

use clipline_lol::LeagueQueueCategory;
use serde::{Deserialize, Serialize};

fn record_default() -> bool {
    true
}

/// Per-category record toggles. Every field defaults to record so upgrades
/// and partial settings files preserve existing behavior.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeagueModeSettings {
    #[serde(default = "record_default")]
    pub record_ranked_solo_duo: bool,
    #[serde(default = "record_default")]
    pub record_ranked_flex: bool,
    #[serde(default = "record_default")]
    pub record_normal: bool,
    #[serde(default = "record_default")]
    pub record_aram: bool,
    #[serde(default = "record_default")]
    pub record_arena: bool,
    #[serde(default = "record_default")]
    pub record_custom: bool,
    #[serde(default = "record_default")]
    pub record_other: bool,
    /// Policy when the local client lookup fails or is unavailable.
    #[serde(default = "record_default")]
    pub record_unknown: bool,
}

impl Default for LeagueModeSettings {
    fn default() -> Self {
        Self {
            record_ranked_solo_duo: true,
            record_ranked_flex: true,
            record_normal: true,
            record_aram: true,
            record_arena: true,
            record_custom: true,
            record_other: true,
            record_unknown: true,
        }
    }
}

impl LeagueModeSettings {
    /// A gate exists only when at least one category is switched off.
    pub fn has_gate(&self) -> bool {
        !(self.record_ranked_solo_duo
            && self.record_ranked_flex
            && self.record_normal
            && self.record_aram
            && self.record_arena
            && self.record_custom
            && self.record_other
            && self.record_unknown)
    }

    /// Whether a detected game with this queue tag may be recorded
    /// automatically. `None` is the unknown tag (lookup failure).
    pub fn allows(&self, category: Option<&LeagueQueueCategory>) -> bool {
        match category {
            Some(LeagueQueueCategory::RankedSoloDuo) => self.record_ranked_solo_duo,
            Some(LeagueQueueCategory::RankedFlex) => self.record_ranked_flex,
            Some(LeagueQueueCategory::Normal) => self.record_normal,
            Some(LeagueQueueCategory::Aram) => self.record_aram,
            Some(LeagueQueueCategory::Arena) => self.record_arena,
            Some(LeagueQueueCategory::Custom) => self.record_custom,
            Some(LeagueQueueCategory::Other) => self.record_other,
            None => self.record_unknown,
        }
    }
}
