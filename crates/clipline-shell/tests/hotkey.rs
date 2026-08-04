use clipline_shell::hotkey::{
    interpret_hotkey_key_event, interpret_hotkey_mouse_event, is_global_shortcut_hotkey,
    normalize_hotkey, parse_hotkey_spec, HotkeyCaptureField, HotkeyCaptureInterpretation,
    HotkeyCaptureKeyEvent, HotkeyCaptureLabels, HotkeyCaptureMouseEvent, HotkeyCaptureOutcome,
    HotkeyCaptureReducer, HotkeyKey, HotkeyModifiers, HotkeySetError,
    MAX_HOTKEY_CAPTURE_TOKEN_BYTES, MAX_HOTKEY_LABEL_BYTES, MODIFIER_ALT, MODIFIER_CONTROL,
    MODIFIER_NOREPEAT, MODIFIER_SHIFT,
};

fn key_event(code: &'static str, modifiers: HotkeyModifiers) -> HotkeyCaptureKeyEvent<'static> {
    HotkeyCaptureKeyEvent {
        code,
        key: "",
        modifiers,
    }
}

fn labels(primary: &str, secondary: Option<&str>) -> HotkeyCaptureLabels {
    HotkeyCaptureLabels {
        primary: primary.into(),
        secondary: secondary.map(str::to_owned),
    }
}

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

#[test]
fn capture_interpretation_matches_shipping_keyboard_and_mouse_vectors() {
    let ctrl = HotkeyModifiers {
        ctrl: true,
        ..HotkeyModifiers::default()
    };
    assert_eq!(
        interpret_hotkey_key_event(&key_event("ControlLeft", ctrl)),
        HotkeyCaptureInterpretation::Pending {
            message: "Now press an F-key, mouse button, or keyboard key."
        }
    );
    assert_eq!(
        interpret_hotkey_key_event(&key_event("Escape", HotkeyModifiers::default())),
        HotkeyCaptureInterpretation::Cancel
    );
    assert_eq!(
        interpret_hotkey_key_event(&key_event("F10", HotkeyModifiers::default())),
        HotkeyCaptureInterpretation::Captured {
            value: "F10".into()
        }
    );
    assert_eq!(
        interpret_hotkey_key_event(&key_event("KeyG", ctrl)),
        HotkeyCaptureInterpretation::Captured {
            value: "Ctrl+G".into()
        }
    );
    assert_eq!(
        interpret_hotkey_key_event(&key_event("F12", HotkeyModifiers::default())),
        HotkeyCaptureInterpretation::Invalid {
            message: "F12 is reserved by Windows for debuggers."
        }
    );
    assert_eq!(
        interpret_hotkey_mouse_event(&HotkeyCaptureMouseEvent {
            button: 4,
            modifiers: HotkeyModifiers {
                alt: true,
                shift: true,
                ..HotkeyModifiers::default()
            },
        }),
        HotkeyCaptureInterpretation::Captured {
            value: "Alt+Shift+Mouse5".into()
        }
    );
}

#[test]
fn capture_reducer_never_mutates_on_pending_invalid_reserved_or_duplicate_input() {
    let mut reducer = HotkeyCaptureReducer::new("Alt+F10", Some("Ctrl+G")).unwrap();
    let before = reducer.settings_labels();
    assert!(matches!(
        reducer.begin(HotkeyCaptureField::Primary),
        HotkeyCaptureOutcome::Recording { .. }
    ));
    assert!(matches!(
        reducer.handle_key(
            HotkeyCaptureField::Primary,
            &key_event(
                "ShiftLeft",
                HotkeyModifiers {
                    shift: true,
                    ..HotkeyModifiers::default()
                }
            )
        ),
        HotkeyCaptureOutcome::Pending { .. }
    ));
    assert_eq!(reducer.settings_labels(), before);
    assert!(matches!(
        reducer.handle_key(
            HotkeyCaptureField::Primary,
            &key_event("F12", HotkeyModifiers::default())
        ),
        HotkeyCaptureOutcome::Rejected { .. }
    ));
    assert_eq!(reducer.settings_labels(), before);
    assert!(matches!(
        reducer.handle_key(HotkeyCaptureField::Primary, &key_event("KeyG", ctrl_only())),
        HotkeyCaptureOutcome::Rejected { .. }
    ));
    assert_eq!(reducer.settings_labels(), before);

    assert_eq!(
        reducer.handle_key(
            HotkeyCaptureField::Primary,
            &key_event(
                "F9",
                HotkeyModifiers {
                    shift: true,
                    ..HotkeyModifiers::default()
                }
            )
        ),
        HotkeyCaptureOutcome::Captured {
            field: HotkeyCaptureField::Primary,
            labels: labels("Shift+F9", Some("Ctrl+G")),
        }
    );
    assert_eq!(
        reducer.settings_labels(),
        labels("Shift+F9", Some("Ctrl+G"))
    );
}

#[test]
fn escape_clears_only_an_active_field_and_never_clears_the_last_hotkey() {
    let mut single = HotkeyCaptureReducer::new("Alt+F10", None).unwrap();
    single.begin(HotkeyCaptureField::Primary);
    assert!(matches!(
        single.handle_key(
            HotkeyCaptureField::Primary,
            &key_event("Escape", HotkeyModifiers::default())
        ),
        HotkeyCaptureOutcome::Rejected { .. }
    ));
    assert_eq!(single.settings_labels(), labels("Alt+F10", None));

    let mut reducer = HotkeyCaptureReducer::new("Alt+F10", Some("Ctrl+G")).unwrap();
    reducer.begin(HotkeyCaptureField::Primary);
    assert_eq!(
        reducer.handle_key(
            HotkeyCaptureField::Primary,
            &key_event("Escape", HotkeyModifiers::default())
        ),
        HotkeyCaptureOutcome::Cleared {
            field: HotkeyCaptureField::Primary,
            labels: labels("Ctrl+G", None),
        }
    );
    assert_eq!(reducer.settings_labels(), labels("Ctrl+G", None));

    reducer.begin(HotkeyCaptureField::Secondary);
    assert!(matches!(
        reducer.handle_key(
            HotkeyCaptureField::Secondary,
            &key_event("Escape", HotkeyModifiers::default())
        ),
        HotkeyCaptureOutcome::Rejected { .. }
    ));
    assert_eq!(reducer.settings_labels(), labels("Ctrl+G", None));
    assert_eq!(reducer.active_field(), Some(HotkeyCaptureField::Secondary));
}

#[test]
fn blur_and_detach_cancel_capture_without_changing_the_draft() {
    let mut reducer = HotkeyCaptureReducer::new("Alt+F10", Some("Ctrl+G")).unwrap();
    let before = reducer.settings_labels();
    reducer.begin(HotkeyCaptureField::Primary);
    assert_eq!(
        reducer.blur(HotkeyCaptureField::Primary),
        HotkeyCaptureOutcome::Canceled
    );
    assert_eq!(reducer.settings_labels(), before);

    reducer.begin(HotkeyCaptureField::Secondary);
    assert_eq!(reducer.detach(), HotkeyCaptureOutcome::Canceled);
    assert_eq!(reducer.settings_labels(), before);
    assert_eq!(reducer.active_field(), None);
}

#[test]
fn inactive_or_stale_field_events_cannot_target_the_current_capture() {
    let mut reducer = HotkeyCaptureReducer::new("Alt+F10", Some("Ctrl+G")).unwrap();
    let before = reducer.settings_labels();
    assert_eq!(
        reducer.handle_key(
            HotkeyCaptureField::Primary,
            &key_event("F9", HotkeyModifiers::default())
        ),
        HotkeyCaptureOutcome::Inactive
    );

    reducer.begin(HotkeyCaptureField::Primary);
    reducer.begin(HotkeyCaptureField::Secondary);
    assert_eq!(
        reducer.handle_key(
            HotkeyCaptureField::Primary,
            &key_event("F9", HotkeyModifiers::default())
        ),
        HotkeyCaptureOutcome::Inactive
    );
    assert_eq!(
        reducer.handle_mouse(
            HotkeyCaptureField::Primary,
            &HotkeyCaptureMouseEvent {
                button: 3,
                modifiers: HotkeyModifiers::default(),
            }
        ),
        HotkeyCaptureOutcome::Inactive
    );
    assert_eq!(
        reducer.blur(HotkeyCaptureField::Primary),
        HotkeyCaptureOutcome::Inactive
    );
    assert_eq!(reducer.active_field(), Some(HotkeyCaptureField::Secondary));
    assert_eq!(reducer.settings_labels(), before);
}

#[test]
fn reset_from_authoritative_settings_is_atomic_and_cancels_capture() {
    let mut reducer = HotkeyCaptureReducer::new("Alt+F10", Some("Ctrl+G")).unwrap();
    reducer.begin(HotkeyCaptureField::Secondary);
    reducer
        .reset_from_settings("Shift+F9", Some("Alt+Mouse5"))
        .unwrap();
    assert_eq!(reducer.active_field(), None);
    assert_eq!(
        reducer.settings_labels(),
        labels("Shift+F9", Some("Alt+Mouse5"))
    );

    let before = reducer.settings_labels();
    let oversized = "X".repeat(MAX_HOTKEY_LABEL_BYTES + 1);
    assert!(reducer
        .reset_from_settings(&oversized, Some("Alt+F10"))
        .is_err());
    assert_eq!(reducer.settings_labels(), before);
}

#[test]
fn every_modifier_and_reserved_or_unsupported_key_preserves_the_active_draft() {
    let mut reducer = HotkeyCaptureReducer::new("Alt+F10", None).unwrap();
    let before = reducer.settings_labels();
    reducer.begin(HotkeyCaptureField::Primary);
    for code in [
        "ControlLeft",
        "ControlRight",
        "AltLeft",
        "AltRight",
        "ShiftLeft",
        "ShiftRight",
    ] {
        assert!(matches!(
            reducer.handle_key(
                HotkeyCaptureField::Primary,
                &key_event(code, HotkeyModifiers::default())
            ),
            HotkeyCaptureOutcome::Pending { .. }
        ));
    }
    for event in [
        key_event("F12", HotkeyModifiers::default()),
        key_event(
            "Tab",
            HotkeyModifiers {
                alt: true,
                ..HotkeyModifiers::default()
            },
        ),
        key_event(
            "F4",
            HotkeyModifiers {
                alt: true,
                ..HotkeyModifiers::default()
            },
        ),
        key_event(
            "Delete",
            HotkeyModifiers {
                ctrl: true,
                alt: true,
                ..HotkeyModifiers::default()
            },
        ),
        key_event("KeyS", HotkeyModifiers::default()),
        key_event("MetaLeft", HotkeyModifiers::default()),
    ] {
        assert!(matches!(
            reducer.handle_key(HotkeyCaptureField::Primary, &event),
            HotkeyCaptureOutcome::Rejected { .. }
        ));
        assert_eq!(reducer.active_field(), Some(HotkeyCaptureField::Primary));
        assert_eq!(reducer.settings_labels(), before);
    }
}

#[test]
fn capture_reducer_rejects_hostile_tokens_and_labels_before_retention() {
    let oversized_label = "A".repeat(MAX_HOTKEY_LABEL_BYTES + 1);
    assert!(matches!(
        HotkeyCaptureReducer::new(&oversized_label, None),
        Err(HotkeySetError::LabelTooLong {
            index: 0,
            bytes,
            maximum: MAX_HOTKEY_LABEL_BYTES,
        }) if bytes == oversized_label.len()
    ));

    let mut reducer = HotkeyCaptureReducer::new("Alt+F10", None).unwrap();
    let before = reducer.settings_labels();
    reducer.begin(HotkeyCaptureField::Primary);
    let oversized_code = "X".repeat(MAX_HOTKEY_CAPTURE_TOKEN_BYTES + 1);
    assert!(matches!(
        reducer.handle_key(
            HotkeyCaptureField::Primary,
            &HotkeyCaptureKeyEvent {
                code: &oversized_code,
                key: "",
                modifiers: HotkeyModifiers::default(),
            }
        ),
        HotkeyCaptureOutcome::Rejected { .. }
    ));
    assert_eq!(reducer.settings_labels(), before);
}

fn ctrl_only() -> HotkeyModifiers {
    HotkeyModifiers {
        ctrl: true,
        ..HotkeyModifiers::default()
    }
}
