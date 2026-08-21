//! Hotkey parsing: optional Ctrl/Alt/Shift + F1-F11/F13-F24, PrintScreen, or
//! selected mouse buttons. F12 is reserved by Windows for debuggers; PrintScreen
//! and F-keys are OS-registered global shortcuts, never low-level-hook keys.

use tauri_plugin_global_shortcut::Shortcut;

pub fn parse_hotkey(raw: &str) -> Result<Shortcut, String> {
    normalize_hotkey(raw)?
        .parse::<Shortcut>()
        .map_err(|e| format!("hotkey: {e}"))
}

pub fn is_global_shortcut_hotkey(raw: &str) -> Result<bool, String> {
    Ok(matches!(
        parse_hotkey_spec(raw)?.key,
        HotkeyKey::Function(_) | HotkeyKey::PrintScreen
    ))
}

pub fn normalize_hotkey(raw: &str) -> Result<String, String> {
    parse_hotkey_spec(raw).map(|spec| spec.normalized())
}

pub(crate) fn parse_hotkey_spec(raw: &str) -> Result<HotkeySpec, String> {
    let mut ctrl = false;
    let mut alt = false;
    let mut shift = false;
    let mut key = None::<HotkeyKey>;

    for part in raw.split('+') {
        let token = part.trim();
        if token.is_empty() {
            return Err("hotkey has an empty part".into());
        }
        match token.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => set_once(&mut ctrl, "Ctrl")?,
            "alt" => set_once(&mut alt, "Alt")?,
            "shift" => set_once(&mut shift, "Shift")?,
            other
                if other.starts_with('f')
                    && !other[1..].is_empty()
                    && other[1..].chars().all(|c| c.is_ascii_digit()) =>
            {
                if key.is_some() {
                    return Err("hotkey has more than one key".into());
                }
                let n = other[1..]
                    .parse::<u8>()
                    .map_err(|_| "hotkey key must be F1-F11 or F13-F24")?;
                if !(1..=24).contains(&n) {
                    return Err("hotkey key must be F1-F11 or F13-F24".into());
                }
                if n == 12 {
                    return Err("F12 is reserved by Windows for debuggers".into());
                }
                key = Some(HotkeyKey::Function(n));
            }
            other => {
                if key.is_some() {
                    return Err("hotkey has more than one key".into());
                }
                if matches!(other, "printscreen" | "prtsc" | "prtscn") {
                    key = Some(HotkeyKey::PrintScreen);
                } else if let Some(mouse) = mouse_key_from_token(other) {
                    key = Some(mouse);
                } else if let Some(keyboard) = keyboard_key_from_token(other) {
                    key = Some(HotkeyKey::Keyboard(keyboard));
                } else {
                    return Err(
                        "hotkey must use F1-F11, F13-F24, PrintScreen, Middle, Mouse4, Mouse5, or Ctrl/Alt/Shift plus a keyboard key".into(),
                    );
                }
            }
        }
    }

    let key = key.ok_or("hotkey needs a key")?;
    validate_hotkey_combination(&key, ctrl, alt, shift)?;

    Ok(HotkeySpec {
        ctrl,
        alt,
        shift,
        key,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HotkeySpec {
    pub(crate) ctrl: bool,
    pub(crate) alt: bool,
    pub(crate) shift: bool,
    pub(crate) key: HotkeyKey,
}

impl HotkeySpec {
    fn normalized(&self) -> String {
        let mut parts = Vec::new();
        if self.ctrl {
            parts.push("Ctrl".to_string());
        }
        if self.alt {
            parts.push("Alt".to_string());
        }
        if self.shift {
            parts.push("Shift".to_string());
        }
        parts.push(self.key.label());
        parts.join("+")
    }
}

fn validate_hotkey_combination(
    key: &HotkeyKey,
    ctrl: bool,
    alt: bool,
    shift: bool,
) -> Result<(), String> {
    match key {
        HotkeyKey::Keyboard(label) => {
            if !ctrl && !alt && !shift {
                return Err("keyboard hotkeys need Ctrl, Alt, or Shift".into());
            }
            match label.as_str() {
                "Tab" if alt => return Err("Alt+Tab is reserved by Windows".into()),
                "Delete" if ctrl && alt => {
                    return Err("Ctrl+Alt+Delete is reserved by Windows".into());
                }
                "Esc" => return Err("Escape is reserved for clearing hotkey capture".into()),
                _ => {}
            }
        }
        HotkeyKey::Function(4) if alt => return Err("Alt+F4 is reserved by Windows".into()),
        _ => {}
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum HotkeyKey {
    Function(u8),
    Keyboard(String),
    PrintScreen,
    Middle,
    Mouse4,
    Mouse5,
}

impl HotkeyKey {
    fn label(&self) -> String {
        match self {
            Self::Function(n) => format!("F{n}"),
            Self::Keyboard(label) => label.clone(),
            Self::PrintScreen => "PrintScreen".to_string(),
            Self::Middle => "Middle".to_string(),
            Self::Mouse4 => "Mouse4".to_string(),
            Self::Mouse5 => "Mouse5".to_string(),
        }
    }
}

fn keyboard_key_from_token(token: &str) -> Option<String> {
    if token.len() == 1 {
        let c = token.as_bytes()[0];
        if c.is_ascii_alphabetic() {
            return Some((c as char).to_ascii_uppercase().to_string());
        }
        if c.is_ascii_digit() {
            return Some(token.to_string());
        }
    }

    let label = match token.to_ascii_lowercase().as_str() {
        "arrowup" | "up" => "ArrowUp",
        "arrowdown" | "down" => "ArrowDown",
        "arrowleft" | "left" => "ArrowLeft",
        "arrowright" | "right" => "ArrowRight",
        "space" => "Space",
        "enter" | "return" => "Enter",
        "tab" => "Tab",
        "backspace" => "Backspace",
        "delete" | "del" => "Delete",
        "insert" | "ins" => "Insert",
        "home" => "Home",
        "end" => "End",
        "pageup" => "PageUp",
        "pagedown" => "PageDown",
        "minus" | "-" => "Minus",
        "equal" | "equals" | "=" => "Equal",
        "bracketleft" | "leftbracket" | "[" => "BracketLeft",
        "bracketright" | "rightbracket" | "]" => "BracketRight",
        "backslash" | "\\" => "Backslash",
        "semicolon" | ";" => "Semicolon",
        "quote" | "apostrophe" | "'" => "Quote",
        "comma" | "," => "Comma",
        "period" | "." => "Period",
        "slash" | "/" => "Slash",
        "backquote" | "grave" | "`" => "Backquote",
        "esc" | "escape" => "Esc",
        _ => return None,
    };
    Some(label.to_string())
}

fn mouse_key_from_token(token: &str) -> Option<HotkeyKey> {
    match token.to_ascii_lowercase().as_str() {
        "middle" => Some(HotkeyKey::Middle),
        "mouse4" => Some(HotkeyKey::Mouse4),
        "mouse5" => Some(HotkeyKey::Mouse5),
        _ => None,
    }
}

fn set_once(slot: &mut bool, name: &str) -> Result<(), String> {
    if *slot {
        return Err(format!("hotkey repeats {name}"));
    }
    *slot = true;
    Ok(())
}
