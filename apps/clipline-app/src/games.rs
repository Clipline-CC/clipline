//! Windows enumeration adapter over shared game matching.

pub use clipline_games::detection::{built_in_game_still_configured, DetectedGame, GameWindowInfo};
pub use clipline_games::windows::list_game_windows;

pub fn game_plugin_catalog() -> Vec<clipline_games::plugin::GamePluginInfo> {
    clipline_games::plugin::catalog(&clipline_settings::icon_cache_dir())
}
