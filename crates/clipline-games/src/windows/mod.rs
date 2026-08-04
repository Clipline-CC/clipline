//! Windows-only game discovery helpers behind safe APIs.

use std::path::PathBuf;

use clipline_capture::windows::{
    enumerate_capturable_windows, enumerate_capturable_windows_with_checkpoint, CapturableWindow,
};
use clipline_settings::{games::GameSettings, CustomGameSettings};

use crate::detection::{self, DetectedGame, GameWindow, GameWindowInfo};
use crate::discovery::{self, DetectedGameCandidate};

pub mod icon;

pub fn list_game_windows() -> Vec<GameWindowInfo> {
    detection::project_game_windows(enumerate_game_windows(), std::process::id())
}

pub fn list_game_windows_with_checkpoint(
    checkpoint: impl FnOnce() -> Result<(), String>,
) -> Result<Vec<GameWindowInfo>, String> {
    let windows = enumerate_capturable_windows_with_checkpoint(checkpoint)?
        .into_iter()
        .map(game_window)
        .collect();
    Ok(detection::project_game_windows(windows, std::process::id()))
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

pub fn detect_installed_games_with_checkpoint(
    existing_custom_games: &[CustomGameSettings],
    checkpoint: impl FnOnce() -> Result<(), String>,
) -> Result<Vec<DetectedGameCandidate>, String> {
    if existing_custom_games.len() > discovery::MAX_DISCOVERED_GAMES {
        return Err(format!(
            "custom game count {} exceeds {}",
            existing_custom_games.len(),
            discovery::MAX_DISCOVERED_GAMES
        ));
    }
    let roots = steam_install_roots()?;
    checkpoint()?;
    let steam_apps = discovery::steam_apps_from_roots(&roots)?;
    let windows = enumerate_capturable_windows_with_checkpoint(|| Ok(()))?
        .into_iter()
        .map(game_window)
        .collect();
    discovery::candidates_from_sources_checked(
        steam_apps,
        windows,
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
    use windows_sys::Win32::Foundation::{ERROR_MORE_DATA, ERROR_SUCCESS};
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegGetValueW, RegOpenKeyExW, HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE,
        RRF_RT_REG_SZ,
    };

    const MAX_REGISTRY_VALUE_BYTES: u32 = 64 * 1024;

    struct Key(HKEY);
    impl Drop for Key {
        fn drop(&mut self) {
            // SAFETY: this handle was returned by RegOpenKeyExW and is owned.
            unsafe {
                RegCloseKey(self.0);
            }
        }
    }

    let subkey = key.strip_prefix("HKCU\\")?;
    let subkey = wide_null(std::ffi::OsStr::new(subkey));
    let value_name_wide = wide_null(std::ffi::OsStr::new(value_name));
    let mut raw = std::ptr::null_mut();
    // SAFETY: pointers reference nul-terminated input and a valid out slot.
    if unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            0,
            KEY_QUERY_VALUE,
            &mut raw,
        )
    } != ERROR_SUCCESS
    {
        return None;
    }
    let key = Key(raw);
    let mut bytes = 0u32;
    // SAFETY: a null data pointer asks Windows for the exact value size.
    let measured = unsafe {
        RegGetValueW(
            key.0,
            std::ptr::null(),
            value_name_wide.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut bytes,
        )
    };
    if !matches!(measured, ERROR_SUCCESS | ERROR_MORE_DATA)
        || bytes == 0
        || bytes > MAX_REGISTRY_VALUE_BYTES
    {
        return None;
    }
    let mut utf16 = vec![0u16; (bytes as usize).div_ceil(2)];
    // SAFETY: the buffer was sized from the bounded measurement above.
    if unsafe {
        RegGetValueW(
            key.0,
            std::ptr::null(),
            value_name_wide.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            utf16.as_mut_ptr().cast(),
            &mut bytes,
        )
    } != ERROR_SUCCESS
    {
        return None;
    }
    let length = utf16
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(utf16.len());
    String::from_utf16(&utf16[..length]).ok()
}

pub(crate) fn wide_null(value: &std::ffi::OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt as _;

    value.encode_wide().chain(std::iter::once(0)).collect()
}
