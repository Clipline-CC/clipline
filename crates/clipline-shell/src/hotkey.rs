//! Stable hotkey grammar and Windows virtual-key mapping.

use thiserror::Error;

pub const MODIFIER_ALT: u32 = 0x0001;
pub const MODIFIER_CONTROL: u32 = 0x0002;
pub const MODIFIER_SHIFT: u32 = 0x0004;
pub const MODIFIER_NOREPEAT: u32 = 0x4000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HotkeySpec {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub key: HotkeyKey,
}

impl HotkeySpec {
    #[must_use]
    pub fn normalized(&self) -> String {
        let mut parts = Vec::with_capacity(4);
        if self.ctrl {
            parts.push("Ctrl".to_owned());
        }
        if self.alt {
            parts.push("Alt".to_owned());
        }
        if self.shift {
            parts.push("Shift".to_owned());
        }
        parts.push(self.key.label());
        parts.join("+")
    }

    #[must_use]
    pub const fn modifier_flags(&self) -> u32 {
        (if self.alt { MODIFIER_ALT } else { 0 })
            | (if self.ctrl { MODIFIER_CONTROL } else { 0 })
            | (if self.shift { MODIFIER_SHIFT } else { 0 })
    }

    #[must_use]
    pub const fn registration_modifier_flags(&self) -> u32 {
        self.modifier_flags() | MODIFIER_NOREPEAT
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HotkeyKey {
    Function(u8),
    Keyboard(KeyboardKey),
    Middle,
    Mouse4,
    Mouse5,
}

impl HotkeyKey {
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Function(number) => format!("F{number}"),
            Self::Keyboard(key) => key.label(),
            Self::Middle => "Middle".to_owned(),
            Self::Mouse4 => "Mouse4".to_owned(),
            Self::Mouse5 => "Mouse5".to_owned(),
        }
    }

    #[must_use]
    pub fn virtual_key_code(&self) -> u32 {
        match self {
            Self::Function(number) => 0x70 + u32::from(*number) - 1,
            Self::Keyboard(key) => key.virtual_key_code(),
            Self::Middle => 0x04,
            Self::Mouse4 => 0x05,
            Self::Mouse5 => 0x06,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyboardKey {
    label: String,
    virtual_key_code: u32,
}

impl KeyboardKey {
    #[must_use]
    pub fn label(&self) -> String {
        self.label.clone()
    }

    #[must_use]
    pub const fn virtual_key_code(&self) -> u32 {
        self.virtual_key_code
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum HotkeyParseError {
    #[error("hotkey has an empty part")]
    EmptyPart,
    #[error("hotkey repeats {0}")]
    RepeatedModifier(&'static str),
    #[error("hotkey has more than one key")]
    MultipleKeys,
    #[error("hotkey key must be F1-F11 or F13-F24")]
    FunctionKeyRange,
    #[error("F12 is reserved by Windows for debuggers")]
    ReservedF12,
    #[error("hotkey must use F1-F11, F13-F24, Middle, Mouse4, Mouse5, or Ctrl/Alt/Shift plus a keyboard key")]
    UnsupportedKey,
    #[error("hotkey needs a key")]
    MissingKey,
    #[error("keyboard hotkeys need Ctrl, Alt, or Shift")]
    KeyboardNeedsModifier,
    #[error("Alt+Tab is reserved by Windows")]
    ReservedAltTab,
    #[error("Ctrl+Alt+Delete is reserved by Windows")]
    ReservedCtrlAltDelete,
    #[error("Escape is reserved for clearing hotkey capture")]
    ReservedEscape,
    #[error("Alt+F4 is reserved by Windows")]
    ReservedAltF4,
}

pub fn parse_hotkey_spec(raw: &str) -> Result<HotkeySpec, HotkeyParseError> {
    let mut ctrl = false;
    let mut alt = false;
    let mut shift = false;
    let mut key = None::<HotkeyKey>;

    for part in raw.split('+') {
        let token = part.trim();
        if token.is_empty() {
            return Err(HotkeyParseError::EmptyPart);
        }
        match token.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => set_once(&mut ctrl, "Ctrl")?,
            "alt" => set_once(&mut alt, "Alt")?,
            "shift" => set_once(&mut shift, "Shift")?,
            other
                if other.starts_with('f')
                    && !other[1..].is_empty()
                    && other[1..]
                        .chars()
                        .all(|character| character.is_ascii_digit()) =>
            {
                if key.is_some() {
                    return Err(HotkeyParseError::MultipleKeys);
                }
                let number = other[1..]
                    .parse::<u8>()
                    .map_err(|_| HotkeyParseError::FunctionKeyRange)?;
                if !(1..=24).contains(&number) {
                    return Err(HotkeyParseError::FunctionKeyRange);
                }
                if number == 12 {
                    return Err(HotkeyParseError::ReservedF12);
                }
                key = Some(HotkeyKey::Function(number));
            }
            other => {
                if key.is_some() {
                    return Err(HotkeyParseError::MultipleKeys);
                }
                if let Some(mouse) = mouse_key_from_token(other) {
                    key = Some(mouse);
                } else if let Some(keyboard) = keyboard_key_from_token(other) {
                    key = Some(HotkeyKey::Keyboard(keyboard));
                } else {
                    return Err(HotkeyParseError::UnsupportedKey);
                }
            }
        }
    }

    let key = key.ok_or(HotkeyParseError::MissingKey)?;
    validate_hotkey_combination(&key, ctrl, alt, shift)?;
    Ok(HotkeySpec {
        ctrl,
        alt,
        shift,
        key,
    })
}

pub fn normalize_hotkey(raw: &str) -> Result<String, HotkeyParseError> {
    parse_hotkey_spec(raw).map(|spec| spec.normalized())
}

pub fn is_global_shortcut_hotkey(raw: &str) -> Result<bool, HotkeyParseError> {
    Ok(matches!(
        parse_hotkey_spec(raw)?.key,
        HotkeyKey::Function(_)
    ))
}

fn validate_hotkey_combination(
    key: &HotkeyKey,
    ctrl: bool,
    alt: bool,
    shift: bool,
) -> Result<(), HotkeyParseError> {
    match key {
        HotkeyKey::Keyboard(key) => {
            if !ctrl && !alt && !shift {
                return Err(HotkeyParseError::KeyboardNeedsModifier);
            }
            match key.label.as_str() {
                "Tab" if alt => return Err(HotkeyParseError::ReservedAltTab),
                "Delete" if ctrl && alt => return Err(HotkeyParseError::ReservedCtrlAltDelete),
                "Esc" => return Err(HotkeyParseError::ReservedEscape),
                _ => {}
            }
        }
        HotkeyKey::Function(4) if alt => return Err(HotkeyParseError::ReservedAltF4),
        _ => {}
    }
    Ok(())
}

fn keyboard_key_from_token(token: &str) -> Option<KeyboardKey> {
    if token.len() == 1 {
        let character = token.as_bytes()[0];
        if character.is_ascii_alphabetic() {
            let uppercase = character.to_ascii_uppercase();
            return Some(KeyboardKey {
                label: (uppercase as char).to_string(),
                virtual_key_code: u32::from(uppercase),
            });
        }
        if character.is_ascii_digit() {
            return Some(KeyboardKey {
                label: token.to_owned(),
                virtual_key_code: u32::from(character),
            });
        }
    }

    let (label, virtual_key_code) = match token.to_ascii_lowercase().as_str() {
        "arrowup" | "up" => ("ArrowUp", 0x26),
        "arrowdown" | "down" => ("ArrowDown", 0x28),
        "arrowleft" | "left" => ("ArrowLeft", 0x25),
        "arrowright" | "right" => ("ArrowRight", 0x27),
        "space" => ("Space", 0x20),
        "enter" | "return" => ("Enter", 0x0D),
        "tab" => ("Tab", 0x09),
        "backspace" => ("Backspace", 0x08),
        "delete" | "del" => ("Delete", 0x2E),
        "insert" | "ins" => ("Insert", 0x2D),
        "home" => ("Home", 0x24),
        "end" => ("End", 0x23),
        "pageup" => ("PageUp", 0x21),
        "pagedown" => ("PageDown", 0x22),
        "minus" | "-" => ("Minus", 0xBD),
        "equal" | "equals" | "=" => ("Equal", 0xBB),
        "bracketleft" | "leftbracket" | "[" => ("BracketLeft", 0xDB),
        "bracketright" | "rightbracket" | "]" => ("BracketRight", 0xDD),
        "backslash" | "\\" => ("Backslash", 0xDC),
        "semicolon" | ";" => ("Semicolon", 0xBA),
        "quote" | "apostrophe" | "'" => ("Quote", 0xDE),
        "comma" | "," => ("Comma", 0xBC),
        "period" | "." => ("Period", 0xBE),
        "slash" | "/" => ("Slash", 0xBF),
        "backquote" | "grave" | "`" => ("Backquote", 0xC0),
        "esc" | "escape" => ("Esc", 0x1B),
        _ => return None,
    };
    Some(KeyboardKey {
        label: label.to_owned(),
        virtual_key_code,
    })
}

fn mouse_key_from_token(token: &str) -> Option<HotkeyKey> {
    match token.to_ascii_lowercase().as_str() {
        "middle" => Some(HotkeyKey::Middle),
        "mouse4" => Some(HotkeyKey::Mouse4),
        "mouse5" => Some(HotkeyKey::Mouse5),
        _ => None,
    }
}

fn set_once(slot: &mut bool, name: &'static str) -> Result<(), HotkeyParseError> {
    if *slot {
        return Err(HotkeyParseError::RepeatedModifier(name));
    }
    *slot = true;
    Ok(())
}
