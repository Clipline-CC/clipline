//! Compatibility adapter over the framework-neutral hotkey grammar.

#[cfg(test)]
pub fn parse_hotkey(raw: &str) -> Result<clipline_shell::hotkey::HotkeySpec, String> {
    clipline_shell::hotkey::parse_hotkey_spec(raw).map_err(|error| error.to_string())
}

pub fn normalize_hotkey(raw: &str) -> Result<String, String> {
    clipline_shell::hotkey::normalize_hotkey(raw).map_err(|error| error.to_string())
}
