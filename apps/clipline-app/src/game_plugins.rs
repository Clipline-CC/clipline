//! First-party supported game registry.
//!
//! A supported game profile describes how Clipline should recognize a game's
//! real in-game window from normal Win32 window/process metadata, plus how its
//! clips should be presented in the review UI. Profiles must stay
//! anti-cheat-safe: no injection, no memory reads, no game-process hooks. Event
//! ingestion stays behind built-in capability names.

use std::path::PathBuf;
use std::sync::{mpsc::Receiver, OnceLock};
use std::time::Instant;

use clipline_capture::windows::CapturableWindow;
use clipline_events::GameId;
use serde::{Deserialize, Serialize};

use crate::markers::PollerMsg;
use crate::settings::{
    GamePluginReviewSettings, GamePluginSettings, GameRecordingMode, GameSettings,
};

pub use crate::game_identity::{LEAGUE_OF_LEGENDS_ID, OSU_ID};
pub const LEAGUE_LIVE_CLIENT_EVENT_SOURCE: &str = "league_live_client";
pub const GAME_PROFILE_SCHEMA_VERSION: u32 = 1;

pub type EventSourceSpawner = fn(GameEventSourceContext) -> Receiver<PollerMsg>;

#[derive(Clone, Debug)]
pub struct GameEventSourceContext {
    pub lol_url: Option<String>,
    pub recording_t0: Instant,
    pub league_game_executable: Option<PathBuf>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct GamePluginInfo {
    pub id: String,
    pub name: String,
    pub summary: String,
    pub default_enabled: bool,
    pub default_recording_mode: GameRecordingMode,
    pub default_review: GamePluginReviewSettings,
    pub event_markers: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub presentation: Option<serde_json::Value>,
    /// Icon for the UI: the profile's bundled icon URL, or a cached icon
    /// extracted from the running game's executable. None when neither exists.
    pub icon: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GameProfileManifest {
    pub schema_version: u32,
    pub id: String,
    pub name: String,
    pub summary: String,
    pub default_enabled: bool,
    pub default_recording_mode: GameRecordingMode,
    #[serde(default)]
    pub icon: Option<GameProfileIcon>,
    pub window_match: WindowMatchRule,
    #[serde(default)]
    pub event_source: Option<String>,
    #[serde(default)]
    pub presentation: Option<serde_json::Value>,
}

impl GameProfileManifest {
    pub fn from_json(json: &str) -> Result<Self, String> {
        let manifest: Self = serde_json::from_str(json).map_err(|e| e.to_string())?;
        manifest.validate()?;
        Ok(manifest)
    }

    fn validate(&self) -> Result<(), String> {
        if self.schema_version != GAME_PROFILE_SCHEMA_VERSION {
            return Err(format!(
                "unsupported game profile schema {}; expected {}",
                self.schema_version, GAME_PROFILE_SCHEMA_VERSION
            ));
        }
        validate_game_profile_id(&self.id)?;
        if self.name.trim().is_empty() {
            return Err("game profile name is required".into());
        }
        if self.summary.trim().is_empty() {
            return Err("game profile summary is required".into());
        }
        self.window_match.validate()?;
        if let Some(event_source) = self.event_source.as_deref() {
            if event_source_spawner(event_source).is_none() {
                return Err(format!("unsupported game event source {event_source:?}"));
            }
        }
        Ok(())
    }

    pub fn match_window<'a>(
        &self,
        windows: &'a [CapturableWindow],
    ) -> Option<&'a CapturableWindow> {
        self.window_match.match_window(windows)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GameProfileIcon {
    UiAsset { path: String },
    File { path: String },
    DataUrl { data: String },
    Extracted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowMatchRule {
    pub exe_name: String,
    #[serde(default = "default_window_selection")]
    pub selection: WindowSelection,
    #[serde(default)]
    pub reject_title_exact: Vec<String>,
    #[serde(default)]
    pub reject_title_contains: Vec<String>,
    #[serde(default)]
    pub require_title_contains: Vec<String>,
}

impl WindowMatchRule {
    fn validate(&self) -> Result<(), String> {
        if self.exe_name.trim().is_empty() {
            return Err("game profile window matcher exe_name is required".into());
        }
        if self
            .reject_title_exact
            .iter()
            .any(|title| title.trim().is_empty())
        {
            return Err("game profile rejected window titles cannot be blank".into());
        }
        if self
            .reject_title_contains
            .iter()
            .any(|title| title.trim().is_empty())
        {
            return Err("game profile rejected window title fragments cannot be blank".into());
        }
        if self
            .require_title_contains
            .iter()
            .any(|title| title.trim().is_empty())
        {
            return Err("game profile required window title fragments cannot be blank".into());
        }
        Ok(())
    }

    fn match_window<'a>(&self, windows: &'a [CapturableWindow]) -> Option<&'a CapturableWindow> {
        let exe_name = self.exe_name.trim();
        match self.selection {
            WindowSelection::LongestTitle => windows
                .iter()
                .filter(|window| window.exe_name.eq_ignore_ascii_case(exe_name))
                .filter(|window| self.allows_title(&window.title))
                .max_by_key(|window| window.title.len()),
        }
    }

    fn allows_title(&self, title: &str) -> bool {
        if self.rejects_title(title) {
            return false;
        }
        if self.require_title_contains.is_empty() {
            return true;
        }

        let title_lower = title.to_ascii_lowercase();
        self.require_title_contains
            .iter()
            .any(|required| title_lower.contains(&required.trim().to_ascii_lowercase()))
    }

    fn rejects_title(&self, title: &str) -> bool {
        let title_lower = title.to_ascii_lowercase();
        self.reject_title_exact
            .iter()
            .any(|rejected| title.trim().eq_ignore_ascii_case(rejected.trim()))
            || self
                .reject_title_contains
                .iter()
                .any(|rejected| title_lower.contains(&rejected.trim().to_ascii_lowercase()))
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WindowSelection {
    LongestTitle,
}

fn default_window_selection() -> WindowSelection {
    WindowSelection::LongestTitle
}

#[derive(Debug, Clone)]
pub struct GamePlugin {
    pub manifest: GameProfileManifest,
}

impl GamePlugin {
    pub fn id(&self) -> &str {
        &self.manifest.id
    }

    pub fn default_settings(&self) -> GamePluginSettings {
        GamePluginSettings {
            enabled: self.manifest.default_enabled,
            recording_mode: self.manifest.default_recording_mode,
            review: Default::default(),
        }
    }

    pub fn settings(&self, settings: &GameSettings) -> GamePluginSettings {
        settings
            .plugins
            .get(self.id())
            .cloned()
            .unwrap_or_else(|| self.default_settings())
    }

    #[cfg(test)]
    fn info(&self) -> GamePluginInfo {
        self.info_with_icon(self.icon_string())
    }

    fn immutable_info(&self) -> GamePluginInfo {
        let icon = if self.uses_extracted_icon() {
            None
        } else {
            self.icon_string()
        };
        self.info_with_icon(icon)
    }

    fn info_with_icon(&self, icon: Option<String>) -> GamePluginInfo {
        let default_settings = self.default_settings();
        GamePluginInfo {
            id: self.id().into(),
            name: self.manifest.name.clone(),
            summary: self.manifest.summary.clone(),
            default_enabled: default_settings.enabled,
            default_recording_mode: default_settings.recording_mode,
            default_review: default_settings.review,
            event_markers: self
                .manifest
                .event_source
                .as_deref()
                .and_then(event_source_spawner)
                .is_some(),
            presentation: self.presentation_value(),
            icon,
        }
    }

    pub fn match_window<'a>(
        &self,
        windows: &'a [CapturableWindow],
    ) -> Option<&'a CapturableWindow> {
        self.manifest.match_window(windows)
    }

    /// Resolve the profile's icon for the UI: prefer the bundled icon, else a
    /// previously-cached icon extracted from the running game's executable.
    fn icon_string(&self) -> Option<String> {
        match self.manifest.icon.as_ref() {
            Some(GameProfileIcon::UiAsset { path }) => Some(path.clone()),
            Some(GameProfileIcon::DataUrl { data }) => Some(data.clone()),
            Some(GameProfileIcon::File { path }) => first_party_asset_data_url(path),
            Some(GameProfileIcon::Extracted) | None => {
                let cache = game_profile_icon_cache_path(self.id())?;
                extracted_icon_data_url(&cache)
            }
        }
    }

    fn presentation_value(&self) -> Option<serde_json::Value> {
        let mut presentation = self.manifest.presentation.clone()?;

        if let Some(marker_kinds) = presentation
            .get_mut("marker_kinds")
            .and_then(serde_json::Value::as_object_mut)
        {
            for config in marker_kinds.values_mut() {
                let Some(icon_value) = config.get_mut("icon") else {
                    continue;
                };
                resolve_profile_asset_value(icon_value);
            }
        }

        if let Some(event_rail_icons) = presentation
            .pointer_mut("/event_rail/icons")
            .and_then(serde_json::Value::as_object_mut)
        {
            for icon_value in event_rail_icons.values_mut() {
                resolve_profile_asset_value(icon_value);
            }
        }

        if let Some(actor_icons) = presentation
            .pointer_mut("/event_rail/actor_icons")
            .and_then(serde_json::Value::as_array_mut)
        {
            for actor_icon in actor_icons {
                if let Some(asset_value) = actor_icon.get_mut("asset") {
                    resolve_profile_asset_value(asset_value);
                }
            }
        }

        if let Some(card_icon_src) = presentation.pointer_mut("/gallery/card/icon/src") {
            resolve_profile_asset_value(card_icon_src);
        }

        Some(presentation)
    }

    fn uses_extracted_icon(&self) -> bool {
        matches!(self.manifest.icon, Some(GameProfileIcon::Extracted) | None)
    }
}

fn extracted_icon_data_url(cache: &std::path::Path) -> Option<String> {
    let bytes = std::fs::read(cache).ok()?;
    Some(crate::game_icon::png_data_url(&bytes))
}

fn resolve_profile_asset_value(icon_value: &mut serde_json::Value) {
    let Some(icon_path) = icon_value.as_str() else {
        return;
    };
    let Some(data_url) = first_party_asset_data_url(icon_path) else {
        return;
    };
    *icon_value = serde_json::Value::String(data_url);
}

fn game_profile_icon_cache_path(profile_id: &str) -> Option<std::path::PathBuf> {
    // Profile ids are simple slugs; reject anything that could escape the dir.
    if profile_id.is_empty() || profile_id.contains(['/', '\\', '.']) {
        return None;
    }
    Some(crate::settings::icon_cache_dir().join(format!("{profile_id}.png")))
}

/// Cache an icon-less profile's icon by extracting it from the running game's
/// executable. No-op for profiles that ship an icon or already have a cache.
/// Cheap to call on the detection poll loop: it short-circuits before any work.
pub fn ensure_plugin_icon_cached(profile_id: &str, exe_path: &str) {
    let needs_extraction = all()
        .iter()
        .any(|profile| profile.id() == profile_id && profile.uses_extracted_icon());
    if !needs_extraction {
        return;
    }
    let Some(cache) = game_profile_icon_cache_path(profile_id) else {
        return;
    };
    if cache.exists() {
        return;
    }
    if let Some(png) = crate::game_icon::extract_exe_icon_png(exe_path) {
        if let Some(parent) = cache.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                tracing::warn!(event = "icon_cache_directory_failed", error = %e);
                return;
            }
        }
        if let Err(e) = std::fs::write(&cache, &png) {
            tracing::warn!(event = "icon_cache_write_failed", error = %e);
        }
    }
}

pub fn all() -> &'static [GamePlugin] {
    static PROFILES: OnceLock<Vec<GamePlugin>> = OnceLock::new();
    PROFILES
        .get_or_init(|| {
            vec![
                GamePlugin {
                    manifest: league_profile_manifest(),
                },
                GamePlugin {
                    manifest: osu_profile_manifest(),
                },
            ]
        })
        .as_slice()
}

fn catalog_base() -> &'static [GamePluginInfo] {
    static CATALOG: OnceLock<Vec<GamePluginInfo>> = OnceLock::new();
    CATALOG
        .get_or_init(|| all().iter().map(GamePlugin::immutable_info).collect())
        .as_slice()
}

pub fn catalog() -> Vec<GamePluginInfo> {
    let mut catalog = catalog_base().to_vec();
    for (profile, info) in all().iter().zip(&mut catalog) {
        if profile.uses_extracted_icon() {
            info.icon = profile.icon_string();
        }
    }
    catalog
}

pub fn plugin_id_for_game_id(game_id: GameId) -> &'static str {
    match game_id {
        GameId::LeagueOfLegends => LEAGUE_OF_LEGENDS_ID,
        GameId::Valorant => crate::game_identity::VALORANT_ID,
        GameId::Cs2 => crate::game_identity::CS2_ID,
        GameId::Osu => OSU_ID,
    }
}

pub fn display_name_for_game_id(game_id: GameId) -> &'static str {
    match game_id {
        GameId::LeagueOfLegends => "League of Legends",
        GameId::Valorant => "Valorant",
        GameId::Cs2 => "CS2",
        GameId::Osu => "osu!",
    }
}

pub fn has_event_source(profile_id: Option<&str>) -> bool {
    let Some(id) = profile_id else {
        return false;
    };
    all().iter().any(|profile| {
        profile.id() == id
            && profile
                .manifest
                .event_source
                .as_deref()
                .and_then(event_source_spawner)
                .is_some()
    })
}

pub fn spawn_event_source(
    profile_id: Option<&str>,
    context: GameEventSourceContext,
) -> Option<Receiver<PollerMsg>> {
    let id = profile_id?;
    let profile = all().iter().find(|profile| profile.id() == id)?;
    let spawn = profile
        .manifest
        .event_source
        .as_deref()
        .and_then(event_source_spawner)?;
    Some(spawn(context))
}

pub fn league_profile_manifest() -> GameProfileManifest {
    GameProfileManifest::from_json(LEAGUE_PROFILE_MANIFEST_JSON)
        .expect("built-in League profile manifest is valid")
}

pub fn osu_profile_manifest() -> GameProfileManifest {
    GameProfileManifest::from_json(OSU_PROFILE_MANIFEST_JSON)
        .expect("built-in osu! profile manifest is valid")
}

fn event_source_spawner(name: &str) -> Option<EventSourceSpawner> {
    match name {
        LEAGUE_LIVE_CLIENT_EVENT_SOURCE => Some(league_of_legends::spawn_event_source),
        _ => None,
    }
}

fn validate_game_profile_id(id: &str) -> Result<(), String> {
    if id.is_empty()
        || id.contains(['/', '\\', '.'])
        || id
            .chars()
            .any(|c| !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'))
    {
        return Err(format!("invalid game profile id {id:?}"));
    }
    Ok(())
}

fn first_party_asset_data_url(path: &str) -> Option<String> {
    let bytes: &[u8] = match path {
        "assets/games/league-of-legends.png" => {
            include_bytes!("../plugin-seeds/league_of_legends/assets/games/league-of-legends.png")
        }
        "assets/games/osu.png" => include_bytes!("../ui/assets/games/osu.png"),
        "assets/markers/kill.png" => {
            include_bytes!("../plugin-seeds/league_of_legends/assets/markers/kill.png")
        }
        "assets/markers/assist.png" => {
            include_bytes!("../plugin-seeds/league_of_legends/assets/markers/assist.png")
        }
        "assets/markers/death.png" => {
            include_bytes!("../plugin-seeds/league_of_legends/assets/markers/death.png")
        }
        "assets/markers/dragon.png" => {
            include_bytes!("../plugin-seeds/league_of_legends/assets/markers/dragon.png")
        }
        "assets/markers/baron.png" => {
            include_bytes!("../plugin-seeds/league_of_legends/assets/markers/baron.png")
        }
        "assets/markers/turret.png" => {
            include_bytes!("../plugin-seeds/league_of_legends/assets/markers/turret.png")
        }
        "assets/event-rail/kill.png" => {
            include_bytes!("../plugin-seeds/league_of_legends/assets/event-rail/kill.png")
        }
        "assets/event-rail/death.png" => {
            include_bytes!("../plugin-seeds/league_of_legends/assets/event-rail/death.png")
        }
        "assets/event-rail/dragon.png" => {
            include_bytes!("../plugin-seeds/league_of_legends/assets/event-rail/dragon.png")
        }
        "assets/event-rail/baron.png" => {
            include_bytes!("../plugin-seeds/league_of_legends/assets/event-rail/baron.png")
        }
        "assets/event-rail/turret.png" => {
            include_bytes!("../plugin-seeds/league_of_legends/assets/event-rail/turret.png")
        }
        "assets/event-rail/minion-100.png" => {
            include_bytes!("../plugin-seeds/league_of_legends/assets/event-rail/minion-100.png")
        }
        "assets/event-rail/minion-200.png" => {
            include_bytes!("../plugin-seeds/league_of_legends/assets/event-rail/minion-200.png")
        }
        _ => return None,
    };
    Some(crate::game_icon::png_data_url(bytes))
}

const LEAGUE_PROFILE_MANIFEST_JSON: &str = r#"{
  "schema_version": 1,
  "id": "league_of_legends",
  "name": "League of Legends",
  "summary": "Auto-records full matches when the in-game window is active.",
  "default_enabled": true,
  "default_recording_mode": "full_session",
  "icon": { "type": "file", "path": "assets/games/league-of-legends.png" },
  "window_match": { "exe_name": "League of Legends.exe", "selection": "longest_title" },
  "event_source": "league_live_client",
  "presentation": {
    "data_dragon": {
      "version": "16.13.1"
    },
    "marker_kinds": {
      "ChampionKill": {
        "category": "kill",
        "icon": "assets/markers/kill.png",
        "rail": { "layout": "duel", "allegiance": "friendly" }
      },
      "ChampionAssist": {
        "category": "assist",
        "icon": "assets/markers/assist.png",
        "rail": { "layout": "duel", "allegiance": "friendly" }
      },
      "ChampionDeath": {
        "category": "death",
        "icon": "assets/markers/death.png",
        "rail": { "layout": "duel", "allegiance": "enemy" }
      },
      "DragonKill": {
        "category": "objective",
        "icon": "assets/markers/dragon.png",
        "rail": { "layout": "actor_event", "allegiance": "actor_team" }
      },
      "BaronKill": {
        "category": "objective",
        "icon": "assets/markers/baron.png",
        "rail": { "layout": "actor_event", "allegiance": "actor_team" }
      },
      "TurretKilled": {
        "category": "structure",
        "icon": "assets/markers/turret.png",
        "rail": { "layout": "actor_event", "allegiance": "actor_team" }
      },
      "HeraldKill": {
        "category": "objective",
        "rail": { "layout": "actor_event", "allegiance": "actor_team" }
      },
      "InhibKilled": {
        "category": "structure",
        "rail": { "layout": "actor_event", "allegiance": "actor_team" }
      },
      "FirstBlood": { "category": "spree" },
      "Multikill": { "category": "spree" },
      "Ace": { "category": "spree" },
      "FirstBrick": { "category": "info" },
      "GameStart": { "category": "info" },
      "MinionsSpawning": { "category": "info" },
      "GameEnd": { "category": "info" }
    },
    "marker_categories": {
      "kill": { "singular": "kill", "plural": "kills", "glyph": "✕" },
      "assist": { "singular": "assist", "plural": "assists", "glyph": "+" },
      "death": { "singular": "death", "plural": "deaths", "glyph": "✕" },
      "spree": { "singular": "spree", "plural": "sprees", "glyph": "★" },
      "objective": { "singular": "objective", "plural": "objectives", "glyph": "◆" },
      "structure": { "singular": "structure", "plural": "structures", "glyph": "▣" },
      "info": { "singular": "event", "plural": "events", "glyph": "•" }
    },
    "gallery": {
      "summary": "player_summary_kda",
      "full_session_title": "summary",
      "card": {
        "title": "summary_for_full_session",
        "title_format": {
          "type": "player_summary_stats",
          "separator": " | ",
          "stats": [
            { "type": "kda" },
            { "type": "cs_per_min", "label": "CS/min" }
          ]
        },
        "icon": {
          "type": "portrait",
          "source": "player_summary.champion_name",
          "label": "Champion",
          "asset_provider": "riot_data_dragon_champion_square",
          "asset_key_format": "data_dragon_champion",
          "asset_aliases": {
            "belveth": "Belveth",
            "cho'gath": "Chogath",
            "dr. mundo": "DrMundo",
            "kai'sa": "Kaisa",
            "kha'zix": "Khazix",
            "k'sante": "KSante",
            "kog'maw": "KogMaw",
            "leblanc": "Leblanc",
            "nunu & willump": "Nunu",
            "rek'sai": "Reksai",
            "renata glasc": "Renata",
            "vel'koz": "Velkoz",
            "wukong": "MonkeyKing"
          }
        }
      }
    },
    "event_rail": {
      "enabled": true,
      "title": "Match events",
      "layout": "kill_feed",
      "icons": {
        "ChampionKill": "assets/event-rail/kill.png",
        "ChampionAssist": "assets/markers/assist.png",
        "ChampionDeath": "assets/event-rail/death.png",
        "DragonKill": "assets/event-rail/dragon.png",
        "BaronKill": "assets/event-rail/baron.png",
        "TurretKilled": "assets/event-rail/turret.png"
      },
      "actor_icons": [
        { "prefix": "Minion_T100", "name": "Minion", "asset": "assets/event-rail/minion-100.png" },
        { "prefix": "Minion_T200", "name": "Minion", "asset": "assets/event-rail/minion-200.png" }
      ]
    },
    "metadata_panel": {
      "enabled": true,
      "fields": [
        {
          "type": "portrait",
          "source": "player_summary.champion_name",
          "label": "Champion",
          "asset_provider": "riot_data_dragon_champion_square",
          "asset_key_format": "data_dragon_champion",
          "asset_aliases": {
            "belveth": "Belveth",
            "cho'gath": "Chogath",
            "dr. mundo": "DrMundo",
            "kai'sa": "Kaisa",
            "kha'zix": "Khazix",
            "k'sante": "KSante",
            "kog'maw": "KogMaw",
            "leblanc": "Leblanc",
            "nunu & willump": "Nunu",
            "rek'sai": "Reksai",
            "renata glasc": "Renata",
            "vel'koz": "Velkoz",
            "wukong": "MonkeyKing"
          }
        },
        {
          "type": "summoner_spells",
          "source": "player_summary.summoner_spells",
          "label": "Summoner spells",
          "asset_provider": "riot_data_dragon_summoner_spell"
        },
        { "type": "kda", "secondary": "kda_ratio" },
        {
          "type": "item_build",
          "source": "player_summary.items",
          "label": "Build",
          "asset_provider": "riot_data_dragon_item",
          "max_items": 7
        }
      ]
    }
  }
}"#;

const OSU_PROFILE_MANIFEST_JSON: &str = r#"{
  "schema_version": 1,
  "id": "osu",
  "name": "osu!",
  "summary": "Auto-records osu!standard sessions and enriches saved sessions with submitted plays.",
  "default_enabled": true,
  "default_recording_mode": "full_session",
  "icon": { "type": "file", "path": "assets/games/osu.png" },
  "window_match": {
    "exe_name": "osu!.exe",
    "selection": "longest_title",
    "reject_title_exact": ["osu! cutting edge"],
    "reject_title_contains": ["updater", "update available", "updating"]
  },
  "presentation": {
    "play_blocks": {
      "enabled": true,
      "source": "plays",
      "estimated_label": "estimated"
    },
    "play_rail": {
      "enabled": true,
      "title": "Set plays",
      "empty": "No osu! plays loaded yet. Add osu! API credentials in osu! settings to fetch submitted plays."
    },
    "gallery": {
      "summary": "osu_set_plays",
      "full_session_title": "osu_session",
      "card": {
        "title": "osu_session_summary",
        "subtitle": "osu_play_count"
      }
    }
  }
}"#;

mod league_of_legends {
    use std::sync::mpsc::Receiver;

    use super::GameEventSourceContext;
    use crate::markers::PollerMsg;

    pub fn spawn_event_source(context: GameEventSourceContext) -> Receiver<PollerMsg> {
        crate::markers::spawn(
            context.lol_url,
            context.recording_t0,
            context.league_game_executable,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clipline_capture::windows::CapturableWindow;
    use clipline_test_utils::TestDir;

    fn window(
        handle: isize,
        title: &str,
        exe_name: &str,
        exe_path: Option<&str>,
    ) -> CapturableWindow {
        CapturableWindow {
            handle,
            title: title.into(),
            process_id: handle as u32,
            exe_name: exe_name.into(),
            exe_path: exe_path.map(str::to_string),
        }
    }

    #[test]
    fn manifest_rejects_unsupported_schema_versions() {
        let json = r#"{
          "schema_version": 2,
          "id": "league_of_legends",
          "name": "League of Legends",
          "summary": "Auto-records full matches when the in-game window is active.",
          "default_enabled": true,
          "default_recording_mode": "full_session",
          "window_match": { "exe_name": "League of Legends.exe", "selection": "longest_title" },
          "event_source": "league_live_client"
        }"#;

        let err = GameProfileManifest::from_json(json).unwrap_err();

        assert!(err.contains("unsupported game profile schema"), "{err}");
    }

    #[test]
    fn unsupported_event_source_names_are_rejected() {
        let json = r#"{
          "schema_version": 1,
          "id": "future_game",
          "name": "Future Game",
          "summary": "Future game profile.",
          "default_enabled": true,
          "default_recording_mode": "full_session",
          "window_match": { "exe_name": "Future.exe", "selection": "longest_title" },
          "event_source": "future_live_client"
        }"#;

        let err = GameProfileManifest::from_json(json).unwrap_err();

        assert!(err.contains("unsupported game event source"), "{err}");
    }

    #[test]
    fn declarative_league_matcher_preserves_longest_title_behavior() {
        let manifest = league_profile_manifest();
        let windows = vec![
            window(1, "League of Legends", "LeagueClientUx.exe", None),
            window(2, "League", "League of Legends.exe", None),
            window(
                3,
                "League of Legends (TM) Client",
                "League of Legends.exe",
                None,
            ),
        ];

        let matched = manifest.match_window(&windows).expect("game window");

        assert_eq!(matched.handle, 3);
        assert_eq!(matched.exe_name, "League of Legends.exe");
    }

    #[test]
    fn league_profile_has_no_install_state_but_keeps_presentation() {
        let profile = all()
            .iter()
            .find(|profile| profile.id() == LEAGUE_OF_LEGENDS_ID)
            .expect("league profile");
        let info = profile.info();

        assert_eq!(info.id, LEAGUE_OF_LEGENDS_ID);
        assert_eq!(info.name, "League of Legends");
        assert!(info.default_enabled);
        assert_eq!(info.default_recording_mode, GameRecordingMode::FullSession);
        assert!(info.event_markers);
        assert!(info
            .icon
            .as_deref()
            .is_some_and(|icon| icon.starts_with("data:image/png;base64,")));

        let presentation = info.presentation.expect("league presentation");
        assert_eq!(
            presentation
                .pointer("/event_rail/title")
                .and_then(serde_json::Value::as_str),
            Some("Match events")
        );
        assert_eq!(
            presentation
                .pointer("/metadata_panel/fields/0/asset_provider")
                .and_then(serde_json::Value::as_str),
            Some("riot_data_dragon_champion_square")
        );
        assert!(presentation
            .pointer("/marker_kinds/ChampionKill/icon")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|icon| icon.starts_with("data:image/png;base64,")));
        assert!(presentation
            .pointer("/marker_kinds/ChampionAssist/icon")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|icon| icon.starts_with("data:image/png;base64,")));
        assert!(presentation
            .pointer("/event_rail/icons/ChampionKill")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|icon| icon.starts_with("data:image/png;base64,")));
        assert!(presentation
            .pointer("/event_rail/actor_icons/1/asset")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|icon| icon.starts_with("data:image/png;base64,")));
    }

    #[test]
    fn osu_profile_is_registered_without_live_event_source() {
        let profile = all()
            .iter()
            .find(|profile| profile.id() == "osu")
            .expect("osu profile");
        let info = profile.info();

        assert_eq!(plugin_id_for_game_id(GameId::Osu), "osu");
        assert_eq!(display_name_for_game_id(GameId::Osu), "osu!");
        assert_eq!(info.name, "osu!");
        assert_eq!(
            info.summary,
            "Auto-records osu!standard sessions and enriches saved sessions with submitted plays."
        );
        assert!(info.default_enabled);
        assert_eq!(info.default_recording_mode, GameRecordingMode::FullSession);
        assert!(!info.event_markers);
        assert!(!has_event_source(Some("osu")));
        assert!(info
            .icon
            .as_deref()
            .is_some_and(|icon| icon.starts_with("data:image/png;base64,")));

        let presentation = info.presentation.expect("osu presentation");
        assert_eq!(
            presentation
                .pointer("/play_blocks/enabled")
                .and_then(serde_json::Value::as_bool),
            Some(true)
        );
        assert_eq!(
            presentation
                .pointer("/play_rail/title")
                .and_then(serde_json::Value::as_str),
            Some("Set plays")
        );
        assert_eq!(
            presentation
                .pointer("/play_rail/empty")
                .and_then(serde_json::Value::as_str),
            Some("No osu! plays loaded yet. Add osu! API credentials in osu! settings to fetch submitted plays.")
        );
        assert_eq!(
            presentation
                .pointer("/gallery/summary")
                .and_then(serde_json::Value::as_str),
            Some("osu_set_plays")
        );

        let windows = vec![
            window(1, "osu!", "osu!.exe", None),
            window(
                2,
                "osu!cuttingedge b20260624",
                "osu!.exe",
                Some(r"C:\Users\dain\AppData\Roaming\osu!\osu!.exe"),
            ),
            window(
                3,
                "osu! - camellia - exit this earth's atomosphere",
                "osu!.exe",
                None,
            ),
        ];
        let matched = profile.match_window(&windows).expect("osu window");
        assert_eq!(matched.handle, 3);
        assert_eq!(matched.exe_name, "osu!.exe");
    }

    #[test]
    fn osu_profile_accepts_cutting_edge_build_title() {
        let profile = all()
            .iter()
            .find(|profile| profile.id() == OSU_ID)
            .expect("osu profile");
        let windows = vec![window(
            1,
            "osu!cuttingedge b20260624",
            "osu!.exe",
            Some(r"C:\Users\dain\AppData\Roaming\osu!\osu!.exe"),
        )];

        let matched = profile
            .match_window(&windows)
            .expect("osu cutting-edge gameplay window");

        assert_eq!(matched.handle, 1);
    }

    #[test]
    fn osu_profile_accepts_stable_play_title_with_extra_spacing() {
        let profile = all()
            .iter()
            .find(|profile| profile.id() == OSU_ID)
            .expect("osu profile");
        let windows = vec![window(
            1,
            "osu!  - ginkiha - EOS [Lycoris]",
            "osu!.exe",
            Some(r"C:\Users\dain\AppData\Roaming\osu!\osu!.exe"),
        )];

        let matched = profile
            .match_window(&windows)
            .expect("osu stable gameplay window");

        assert_eq!(matched.handle, 1);
    }

    #[test]
    fn osu_profile_accepts_stable_idle_title() {
        let profile = all()
            .iter()
            .find(|profile| profile.id() == OSU_ID)
            .expect("osu profile");
        let windows = vec![window(
            1,
            "osu!",
            "osu!.exe",
            Some(r"C:\Users\dain\AppData\Roaming\osu!\osu!.exe"),
        )];

        let matched = profile
            .match_window(&windows)
            .expect("osu stable idle window");

        assert_eq!(matched.handle, 1);
    }

    #[test]
    fn osu_profile_ignores_updater_client_windows() {
        let profile = all()
            .iter()
            .find(|profile| profile.id() == OSU_ID)
            .expect("osu profile");

        for title in [
            "osu! updater",
            "osu! cutting edge",
            "osu! update available",
            "osu! updating",
        ] {
            let windows = vec![window(
                1,
                title,
                "osu!.exe",
                Some(r"C:\Users\dain\AppData\Local\osu!\osu!.exe"),
            )];

            assert!(
                profile.match_window(&windows).is_none(),
                "{title:?} should not be treated as an osu! game window"
            );
        }
    }

    #[test]
    fn profile_records_and_immutable_catalog_data_are_cached() {
        let first_profiles = all();
        let second_profiles = all();
        assert_eq!(first_profiles.as_ptr(), second_profiles.as_ptr());

        let first_base = catalog_base();
        let second_base = catalog_base();
        assert_eq!(first_base.as_ptr(), second_base.as_ptr());
        let first_catalog = catalog();
        let second_catalog = catalog();
        assert_ne!(first_catalog.as_ptr(), second_catalog.as_ptr());
        assert!(first_catalog[0]
            .presentation
            .as_ref()
            .and_then(|presentation| presentation.pointer("/event_rail/icons/ChampionKill"))
            .and_then(serde_json::Value::as_str)
            .is_some_and(|icon| icon.starts_with("data:image/png;base64,")));
        assert!(first_catalog[0]
            .presentation
            .as_ref()
            .and_then(|presentation| presentation.pointer("/event_rail/actor_icons/0/asset"))
            .and_then(serde_json::Value::as_str)
            .is_some_and(|icon| icon.starts_with("data:image/png;base64,")));
    }

    #[test]
    fn extracted_icon_resolver_observes_a_file_created_after_a_missing_read() {
        let dir = TestDir::new("clipline-game-plugins", "late-extracted-icon");
        let cache = dir.path().join("future-game.png");

        assert!(extracted_icon_data_url(&cache).is_none());
        std::fs::write(&cache, [0x89, b'P', b'N', b'G', 1, 2, 3, 4]).unwrap();

        assert!(extracted_icon_data_url(&cache)
            .as_deref()
            .is_some_and(|icon| icon.starts_with("data:image/png;base64,")));
    }

    #[test]
    fn league_profile_declares_categories_for_all_live_client_kinds() {
        // The review filters key on marker categories, so every EventKind the
        // Live Client poller can emit needs a deliberate category — otherwise
        // it silently degrades to "info" and disappears from both surfaces.
        let manifest = league_profile_manifest();
        let presentation = manifest.presentation.expect("presentation");
        let kinds = [
            ("GameStart", "info"),
            ("MinionsSpawning", "info"),
            ("FirstBrick", "info"),
            ("TurretKilled", "structure"),
            ("InhibKilled", "structure"),
            ("DragonKill", "objective"),
            ("HeraldKill", "objective"),
            ("BaronKill", "objective"),
            ("ChampionKill", "kill"),
            ("ChampionAssist", "assist"),
            ("ChampionDeath", "death"),
            ("Multikill", "spree"),
            ("Ace", "spree"),
            ("FirstBlood", "spree"),
            ("GameEnd", "info"),
        ];
        for (kind, category) in kinds {
            assert_eq!(
                presentation
                    .pointer(&format!("/marker_kinds/{kind}/category"))
                    .and_then(serde_json::Value::as_str),
                Some(category),
                "League profile should categorize {kind}"
            );
        }
    }

    #[test]
    fn league_profile_declares_data_dragon_portrait_provider() {
        let manifest = league_profile_manifest();
        let presentation = manifest.presentation.expect("presentation");

        assert_eq!(
            presentation
                .pointer("/data_dragon/version")
                .and_then(serde_json::Value::as_str),
            Some("16.13.1")
        );
        assert_eq!(
            presentation
                .pointer("/metadata_panel/fields/0/asset_provider")
                .and_then(serde_json::Value::as_str),
            Some("riot_data_dragon_champion_square")
        );
        assert_eq!(
            presentation
                .pointer("/metadata_panel/fields/0/asset_aliases/wukong")
                .and_then(serde_json::Value::as_str),
            Some("MonkeyKing")
        );
    }

    #[test]
    fn game_id_bridge_keeps_existing_ids() {
        assert_eq!(
            plugin_id_for_game_id(GameId::LeagueOfLegends),
            LEAGUE_OF_LEGENDS_ID
        );
        assert_eq!(plugin_id_for_game_id(GameId::Valorant), "valorant");
        assert_eq!(plugin_id_for_game_id(GameId::Cs2), "cs2");
        assert_eq!(plugin_id_for_game_id(GameId::Osu), "osu");
    }
}
