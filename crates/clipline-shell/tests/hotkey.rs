use clipline_shell::hotkey::{
    is_global_shortcut_hotkey, normalize_hotkey, parse_hotkey_spec, HotkeyKey, MODIFIER_ALT,
    MODIFIER_CONTROL, MODIFIER_NOREPEAT, MODIFIER_SHIFT,
};

#[test]
fn accepted_legacy_vectors_keep_exact_labels() {
    for (raw, expected) in [
        ("alt+f10", "Alt+F10"),
        ("control+shift+f9", "Ctrl+Shift+F9"),
        ("ctrl+g", "Ctrl+G"),
        ("alt+shift+arrowleft", "Alt+Shift+ArrowLeft"),
        ("ctrl+1", "Ctrl+1"),
        ("ctrl+space", "Ctrl+Space"),
        ("ctrl+/", "Ctrl+Slash"),
        ("ctrl+mouse4", "Ctrl+Mouse4"),
        ("alt+mouse5", "Alt+Mouse5"),
        ("shift+middle", "Shift+Middle"),
        ("mouse4", "Mouse4"),
        ("Mouse5", "Mouse5"),
        ("Middle", "Middle"),
        ("f1", "F1"),
        ("F11", "F11"),
        ("f13", "F13"),
        ("F24", "F24"),
    ] {
        assert_eq!(normalize_hotkey(raw).unwrap(), expected, "{raw}");
    }
}

#[test]
fn keyboard_aliases_and_punctuation_have_stable_virtual_keys() {
    for (raw, label, virtual_key) in [
        ("Ctrl+Up", "Ctrl+ArrowUp", 0x26),
        ("Ctrl+Down", "Ctrl+ArrowDown", 0x28),
        ("Ctrl+Left", "Ctrl+ArrowLeft", 0x25),
        ("Ctrl+Right", "Ctrl+ArrowRight", 0x27),
        ("Ctrl+Return", "Ctrl+Enter", 0x0D),
        ("Ctrl+Backspace", "Ctrl+Backspace", 0x08),
        ("Ctrl+Del", "Ctrl+Delete", 0x2E),
        ("Ctrl+Ins", "Ctrl+Insert", 0x2D),
        ("Ctrl+Home", "Ctrl+Home", 0x24),
        ("Ctrl+End", "Ctrl+End", 0x23),
        ("Ctrl+PageUp", "Ctrl+PageUp", 0x21),
        ("Ctrl+PageDown", "Ctrl+PageDown", 0x22),
        ("Ctrl+-", "Ctrl+Minus", 0xBD),
        ("Ctrl+=", "Ctrl+Equal", 0xBB),
        ("Ctrl+[", "Ctrl+BracketLeft", 0xDB),
        ("Ctrl+]", "Ctrl+BracketRight", 0xDD),
        ("Ctrl+\\", "Ctrl+Backslash", 0xDC),
        ("Ctrl+;", "Ctrl+Semicolon", 0xBA),
        ("Ctrl+'", "Ctrl+Quote", 0xDE),
        ("Ctrl+,", "Ctrl+Comma", 0xBC),
        ("Ctrl+.", "Ctrl+Period", 0xBE),
        ("Ctrl+/", "Ctrl+Slash", 0xBF),
        ("Ctrl+`", "Ctrl+Backquote", 0xC0),
    ] {
        let spec = parse_hotkey_spec(raw).unwrap();
        assert_eq!(spec.normalized(), label, "{raw}");
        assert_eq!(spec.key.virtual_key_code(), virtual_key, "{raw}");
    }
}

#[test]
fn modifiers_and_function_mouse_virtual_keys_are_stable() {
    let spec = parse_hotkey_spec("Ctrl+Alt+Shift+F24").unwrap();
    assert_eq!(
        spec.modifier_flags(),
        MODIFIER_CONTROL | MODIFIER_ALT | MODIFIER_SHIFT
    );
    assert_eq!(
        spec.registration_modifier_flags(),
        MODIFIER_CONTROL | MODIFIER_ALT | MODIFIER_SHIFT | MODIFIER_NOREPEAT
    );
    assert_eq!(spec.key, HotkeyKey::Function(24));
    assert_eq!(spec.key.virtual_key_code(), 0x87);
    assert_eq!(
        parse_hotkey_spec("Middle").unwrap().key.virtual_key_code(),
        0x04
    );
    assert_eq!(
        parse_hotkey_spec("Mouse4").unwrap().key.virtual_key_code(),
        0x05
    );
    assert_eq!(
        parse_hotkey_spec("Mouse5").unwrap().key.virtual_key_code(),
        0x06
    );
}

#[test]
fn reserved_duplicate_and_unsupported_vectors_keep_exact_errors() {
    for (raw, expected) in [
        ("F12", "F12 is reserved by Windows for debuggers"),
        ("Alt+F4", "Alt+F4 is reserved by Windows"),
        ("Alt+Tab", "Alt+Tab is reserved by Windows"),
        ("Ctrl+Alt+Delete", "Ctrl+Alt+Delete is reserved by Windows"),
        (
            "Ctrl+Shift+Esc",
            "Escape is reserved for clearing hotkey capture",
        ),
        ("Ctrl+Ctrl+F10", "hotkey repeats Ctrl"),
        ("F9+F10", "hotkey has more than one key"),
        ("Ctrl++F10", "hotkey has an empty part"),
        ("Ctrl", "hotkey needs a key"),
        ("S", "keyboard hotkeys need Ctrl, Alt, or Shift"),
        ("1", "keyboard hotkeys need Ctrl, Alt, or Shift"),
        ("Slash", "keyboard hotkeys need Ctrl, Alt, or Shift"),
    ] {
        assert_eq!(
            normalize_hotkey(raw).unwrap_err().to_string(),
            expected,
            "{raw}"
        );
    }
    for raw in ["Mouse1", "RightMouse", "forward", "F0", "F25", "nope"] {
        assert!(normalize_hotkey(raw).is_err(), "{raw}");
    }
}

#[test]
fn only_function_keys_use_the_legacy_global_shortcut_path() {
    assert!(is_global_shortcut_hotkey("Alt+F10").unwrap());
    assert!(!is_global_shortcut_hotkey("Ctrl+G").unwrap());
    assert!(!is_global_shortcut_hotkey("Mouse4").unwrap());
}
