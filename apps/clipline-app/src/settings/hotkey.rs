//! Compatibility adapter over the framework-neutral hotkey grammar.

use tauri_plugin_global_shortcut::Shortcut;

pub(crate) use clipline_shell::hotkey::{HotkeyKey, HotkeySpec};

pub fn parse_hotkey(raw: &str) -> Result<Shortcut, String> {
    normalize_hotkey(raw)?
        .parse::<Shortcut>()
        .map_err(|error| format!("hotkey: {error}"))
}

pub fn is_global_shortcut_hotkey(raw: &str) -> Result<bool, String> {
    clipline_shell::hotkey::is_global_shortcut_hotkey(raw).map_err(|error| error.to_string())
}

pub fn normalize_hotkey(raw: &str) -> Result<String, String> {
    clipline_shell::hotkey::normalize_hotkey(raw).map_err(|error| error.to_string())
}

pub(crate) fn parse_hotkey_spec(raw: &str) -> Result<HotkeySpec, String> {
    clipline_shell::hotkey::parse_hotkey_spec(raw).map_err(|error| error.to_string())
}
