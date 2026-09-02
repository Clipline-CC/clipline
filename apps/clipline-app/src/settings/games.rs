//! Game detection settings: built-in plugin state and custom game rules.
//! Owns the legacy `recording_mode` migration (a top-level field on `games`
//! that applied to every custom game) via a custom `Deserialize`.

use std::collections::{BTreeMap, HashMap, HashSet};

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

impl From<GameRecordingMode> for crate::service::RecordingMode {
    fn from(value: GameRecordingMode) -> Self {
        match value {
            GameRecordingMode::FullSession => Self::FullSession,
            GameRecordingMode::ReplaysOnly => Self::ReplaysOnly,
        }
    }
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
    #[serde(default = "default_enabled")]
    pub auto_detect_steam_launches: bool,
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
    #[serde(default = "default_enabled")]
    auto_detect_steam_launches: bool,
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
            auto_detect_steam_launches: true,
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
            auto_detect_steam_launches: wire.auto_detect_steam_launches,
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
            .filter(|game| crate::game_identity::validate_custom_game_id(&game.id).is_ok())
            .map(|game| game.id.clone())
            .collect::<HashSet<_>>();
        for game in &mut self.custom_games {
            if crate::game_identity::validate_custom_game_id(&game.id).is_err() {
                let legacy_id = game.id.clone();
                game.id = crate::game_identity::unique_migrated_custom_game_id(
                    &game.id,
                    &game.name,
                    &mut occupied,
                );
                if !legacy_id.is_empty() && !game.legacy_ids.contains(&legacy_id) {
                    game.legacy_ids.push(legacy_id);
                }
                game.normalize();
            }
        }
        dedupe_custom_games(&mut self.custom_games);
    }
}

fn dedupe_custom_games(games: &mut Vec<CustomGameSettings>) {
    let mut kept = Vec::with_capacity(games.len());
    let mut indexes = HashMap::new();
    for game in std::mem::take(games).into_iter().rev() {
        let Some(key) = custom_game_match_key(&game) else {
            kept.push(game);
            continue;
        };
        if let Some(index) = indexes.get(&key).copied() {
            let keeper: &mut CustomGameSettings = &mut kept[index];
            if game.id != keeper.id {
                keeper.legacy_ids.push(game.id);
            }
            keeper.legacy_ids.extend(game.legacy_ids);
            keeper.normalize();
        } else {
            indexes.insert(key, kept.len());
            kept.push(game);
        }
    }
    kept.reverse();
    *games = kept;
}

fn custom_game_match_key(game: &CustomGameSettings) -> Option<(String, String, String)> {
    let path = game
        .process_path
        .as_deref()
        .map(crate::games::path_key)
        .unwrap_or_default();
    let exe = game.exe_name.trim();
    let title = game.window_title.trim();
    if path.is_empty() && exe.is_empty() && title.is_empty() {
        return None;
    }
    Some((
        path,
        exe.to_ascii_lowercase(),
        title.to_ascii_lowercase(),
    ))
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
