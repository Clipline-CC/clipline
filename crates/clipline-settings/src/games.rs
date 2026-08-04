//! Game detection settings: built-in plugin state and custom game rules.
//! Owns the legacy `recording_mode` migration (a top-level field on `games`
//! that applied to every custom game) via a custom `Deserialize`.

use std::collections::{BTreeMap, HashSet};

use serde::{Deserialize, Serialize};

use super::types::CustomGameSettings;

fn default_enabled() -> bool {
    true
}

fn default_disabled() -> bool {
    false
}

fn default_game_recording_mode_full_session() -> GameRecordingMode {
    GameRecordingMode::FullSession
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GameRecordingMode {
    FullSession,
    #[default]
    ReplaysOnly,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GamePluginSettings {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default = "default_game_recording_mode_full_session")]
    pub recording_mode: GameRecordingMode,
    #[serde(default)]
    pub review: GamePluginReviewSettings,
}

impl Default for GamePluginSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            recording_mode: GameRecordingMode::FullSession,
            review: GamePluginReviewSettings::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GamePluginReviewSettings {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub match_events: MatchEventSettings,
    #[serde(default)]
    pub timeline_markers: TimelineMarkerSettings,
}

impl Default for GamePluginReviewSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            match_events: MatchEventSettings::default(),
            timeline_markers: TimelineMarkerSettings::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MatchEventSettings {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default = "default_enabled")]
    pub user_kills: bool,
    #[serde(default = "default_enabled")]
    pub user_deaths: bool,
    #[serde(default = "default_enabled")]
    pub user_assists: bool,
    #[serde(default = "default_enabled")]
    pub team_kills: bool,
    #[serde(default = "default_enabled")]
    pub team_deaths: bool,
    #[serde(default = "default_enabled")]
    pub enemy_kills: bool,
    #[serde(default = "default_enabled")]
    pub enemy_deaths: bool,
    #[serde(default = "default_enabled")]
    pub objectives: bool,
    #[serde(default = "default_enabled")]
    pub turrets: bool,
}

impl Default for MatchEventSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            user_kills: true,
            user_deaths: true,
            user_assists: true,
            team_kills: true,
            team_deaths: true,
            enemy_kills: true,
            enemy_deaths: true,
            objectives: true,
            turrets: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TimelineMarkerSettings {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default = "default_enabled")]
    pub user_kills: bool,
    #[serde(default = "default_enabled")]
    pub user_deaths: bool,
    #[serde(default = "default_enabled")]
    pub user_assists: bool,
    #[serde(default = "default_enabled")]
    pub objectives: bool,
    #[serde(default = "default_enabled")]
    pub turrets: bool,
}

impl Default for TimelineMarkerSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            user_kills: true,
            user_deaths: true,
            user_assists: true,
            objectives: true,
            turrets: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct GameSettings {
    #[serde(default = "default_enabled")]
    pub auto_detect: bool,
    #[serde(default = "default_disabled")]
    pub pause_when_no_game: bool,
    #[serde(default)]
    pub plugins: BTreeMap<String, GamePluginSettings>,
    #[serde(default)]
    pub custom_games: Vec<CustomGameSettings>,
}

#[derive(Deserialize)]
struct GameSettingsWire {
    #[serde(default = "default_enabled")]
    auto_detect: bool,
    #[serde(default = "default_disabled")]
    pause_when_no_game: bool,
    #[serde(default)]
    plugins: BTreeMap<String, GamePluginSettings>,
    #[serde(default, rename = "recording_mode")]
    legacy_recording_mode: Option<GameRecordingMode>,
    #[serde(default)]
    custom_games: Vec<CustomGameSettings>,
}

impl Default for GameSettings {
    fn default() -> Self {
        Self {
            auto_detect: true,
            pause_when_no_game: false,
            plugins: BTreeMap::new(),
            custom_games: Vec::new(),
        }
    }
}

impl<'de> Deserialize<'de> for GameSettings {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let mut wire = GameSettingsWire::deserialize(deserializer)?;
        if let Some(mode) = wire.legacy_recording_mode {
            for game in &mut wire.custom_games {
                game.recording_mode = mode;
            }
        }
        Ok(Self {
            auto_detect: wire.auto_detect,
            pause_when_no_game: wire.pause_when_no_game,
            plugins: wire.plugins,
            custom_games: wire.custom_games,
        })
    }
}

impl GameSettings {
    pub fn normalize(&mut self) {
        self.plugins = std::mem::take(&mut self.plugins)
            .into_iter()
            .map(|(id, settings)| (normalize_game_plugin_id(&id), settings))
            .filter(|(id, _)| !id.is_empty())
            .collect();
        for game in &mut self.custom_games {
            game.normalize();
        }
        let mut occupied = self
            .custom_games
            .iter()
            .filter(|game| validate_custom_game_id(&game.id).is_ok())
            .map(|game| game.id.clone())
            .collect::<HashSet<_>>();
        for game in &mut self.custom_games {
            if validate_custom_game_id(&game.id).is_err() {
                let legacy_id = game.id.clone();
                game.id = unique_migrated_custom_game_id(&game.id, &game.name, &mut occupied);
                if !legacy_id.is_empty() && !game.legacy_ids.contains(&legacy_id) {
                    game.legacy_ids.push(legacy_id);
                }
                game.normalize();
            }
        }
    }
}

fn normalize_game_plugin_id(raw: &str) -> String {
    raw.trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect::<String>()
        .split('_')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("_")
}

pub const LEAGUE_OF_LEGENDS_ID: &str = "league_of_legends";
pub const VALORANT_ID: &str = "valorant";
pub const CS2_ID: &str = "cs2";
pub const OSU_ID: &str = "osu";
pub const BUILT_IN_GAME_IDS: &[&str] = &[LEAGUE_OF_LEGENDS_ID, VALORANT_ID, CS2_ID, OSU_ID];
const CUSTOM_PREFIX: &str = "custom-";
const MIGRATED_PREFIX: &str = "custom-migrated-";
const MAX_CUSTOM_ID_LEN: usize = 96;

pub fn validate_custom_game_id(id: &str) -> Result<(), String> {
    if BUILT_IN_GAME_IDS.contains(&id) {
        return Err(format!(
            "custom game id {id:?} is reserved for a built-in game"
        ));
    }
    if id.len() > MAX_CUSTOM_ID_LEN {
        return Err(format!(
            "custom game id must be at most {MAX_CUSTOM_ID_LEN} characters"
        ));
    }
    let Some(slug) = id.strip_prefix(CUSTOM_PREFIX) else {
        return Err(format!(
            "custom game id {id:?} must use the {CUSTOM_PREFIX} namespace"
        ));
    };
    if slug.is_empty()
        || slug.starts_with('-')
        || slug.ends_with('-')
        || slug.contains("--")
        || !slug
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(format!("custom game id {id:?} is not a canonical slug"));
    }
    Ok(())
}

pub fn migrated_custom_game_id(raw_id: &str, fallback_name: &str) -> String {
    let source = if raw_id.trim().is_empty() {
        fallback_name
    } else {
        raw_id
    };
    let max_slug_len = MAX_CUSTOM_ID_LEN - MIGRATED_PREFIX.len();
    let mut slug = source
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    slug.truncate(max_slug_len);
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        slug.push_str("game");
    }
    format!("{MIGRATED_PREFIX}{slug}")
}

pub fn unique_migrated_custom_game_id(
    raw_id: &str,
    fallback_name: &str,
    occupied: &mut HashSet<String>,
) -> String {
    let base = migrated_custom_game_id(raw_id, fallback_name);
    let mut candidate = base.clone();
    let mut suffix = 2_u32;
    while occupied.contains(&candidate) {
        let suffix_text = format!("-{suffix}");
        let stem_len = MAX_CUSTOM_ID_LEN - suffix_text.len();
        candidate = format!("{}{suffix_text}", &base[..base.len().min(stem_len)]);
        suffix += 1;
    }
    occupied.insert(candidate.clone());
    candidate
}
