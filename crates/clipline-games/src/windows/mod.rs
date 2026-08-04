//! Windows-only game discovery helpers behind safe APIs.

use std::path::PathBuf;

use clipline_capture::windows::{enumerate_capturable_windows, CapturableWindow};
use clipline_settings::{games::GameSettings, CustomGameSettings};

use crate::detection::{self, DetectedGame, GameWindow, GameWindowInfo};
use crate::discovery::{self, DetectedGameCandidate};

pub mod icon;

pub fn list_game_windows() -> Vec<GameWindowInfo> {
    detection::project_game_windows(enumerate_game_windows(), std::process::id())
}

pub fn detect_active_game(
    settings: &GameSettings,
    icon_cache_dir: &std::path::Path,
) -> Option<DetectedGame> {
    if !detection::has_enabled_games(settings) {
        return None;
    }
    let windows = enumerate_game_windows();
    let detected = detection::detect_active_game_from_windows(settings, windows.clone());
    if let Some(detected) = detected.as_ref() {
        if let Some(profile_id) = detected.identity.plugin_id() {
            if let Some(exe_path) = windows
                .iter()
                .find(|window| window.handle == detected.hwnd)
                .and_then(|window| window.exe_path.as_deref())
            {
                crate::plugin::ensure_plugin_icon_cached(icon_cache_dir, profile_id, exe_path);
            }
        }
    }
    detected
}

pub fn detect_installed_games(
    existing_custom_games: &[CustomGameSettings],
) -> Vec<DetectedGameCandidate> {
    let steam_apps = steam_install_roots()
        .and_then(|roots| discovery::steam_apps_from_roots(&roots))
        .unwrap_or_default();
    discovery::candidates_from_sources(
        steam_apps,
        enumerate_game_windows(),
        existing_custom_games,
        icon::extract_exe_icon_data_url,
    )
}

pub fn enumerate_game_windows() -> Vec<GameWindow> {
    enumerate_capturable_windows()
        .into_iter()
        .map(game_window)
        .collect()
}

fn game_window(window: CapturableWindow) -> GameWindow {
    GameWindow {
        handle: window.handle,
        title: window.title,
        process_id: window.process_id,
        exe_name: window.exe_name,
        exe_path: window.exe_path,
    }
}

fn steam_install_roots() -> Result<Vec<PathBuf>, String> {
    let mut roots = Vec::new();
    if let Some(path) = query_reg_sz(r"HKCU\Software\Valve\Steam", "SteamPath") {
        discovery::add_unique_path(&mut roots, PathBuf::from(path.replace('/', "\\")));
    }
    if let Some(program_files_x86) = std::env::var_os("ProgramFiles(x86)") {
        discovery::add_unique_path(&mut roots, PathBuf::from(program_files_x86).join("Steam"));
    }
    if let Some(program_files) = std::env::var_os("ProgramFiles") {
        discovery::add_unique_path(&mut roots, PathBuf::from(program_files).join("Steam"));
    }
    Ok(roots.into_iter().filter(|path| path.exists()).collect())
}

fn query_reg_sz(key: &str, value_name: &str) -> Option<String> {
    use std::os::windows::process::CommandExt as _;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let output = std::process::Command::new("reg.exe")
        .args(["query", key, "/v", value_name])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    discovery::parse_reg_sz_output(&String::from_utf8_lossy(&output.stdout), value_name)
}

pub(crate) fn wide_null(value: &std::ffi::OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt as _;

    value.encode_wide().chain(std::iter::once(0)).collect()
}
