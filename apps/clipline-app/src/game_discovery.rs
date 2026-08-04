//! Compatibility adapter over shared Windows game discovery.

pub use clipline_games::discovery::DetectedGameCandidate;

pub fn detect_installed_games(
    existing_custom_games: &[clipline_settings::CustomGameSettings],
) -> Vec<DetectedGameCandidate> {
    clipline_games::windows::detect_installed_games(existing_custom_games)
}
