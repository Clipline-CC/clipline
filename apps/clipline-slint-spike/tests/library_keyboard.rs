use std::fs;
use std::path::Path;

fn library_source() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("ui/library.slint");
    fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()))
}

fn dialogs_source() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("ui/dialogs.slint");
    fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()))
}

#[test]
fn catalog_keyboard_contract_is_explicit_and_bounded_to_the_active_page() {
    let source = library_source();

    for contract in [
        "forward-focus: catalog-keyboard",
        "key-pressed(event)",
        "event.text == Key.UpArrow",
        "event.text == Key.DownArrow",
        "event.text == Key.LeftArrow",
        "event.text == Key.RightArrow",
        "event.text == Key.Return",
        "event.text == Key.Space",
        "event.text == Key.Menu || (event.text == Key.F10 && event.modifiers.shift)",
        "event.text == \"a\" && event.modifiers.control",
        "event.text == Key.PageUp",
        "event.text == Key.PageDown",
        "event.text == Key.Escape",
        "root.focused-row = max(0, root.focused-row - 1)",
        "root.focused-row = min(root.rows.length - 1, root.focused-row + 1)",
        "root.open-row(root.focused-row)",
        "root.toggle-row(root.focused-row)",
        "root.open-context(root.focused-row)",
        "root.select-visible()",
        "root.escape()",
    ] {
        assert!(
            source.contains(contract),
            "missing keyboard contract: {contract}"
        );
    }
}

#[test]
fn destructive_confirmation_rejects_key_repeat() {
    let library = library_source();
    let dialogs = dialogs_source();
    assert!(dialogs.contains("if (!root.dialog.destructive || !event.repeat)"));
    assert!(dialogs.contains("root.confirm-dialog()"));
    assert!(library.contains("if (root.dialog-open)"));
    assert!(library.contains("return accept"));
    assert!(!library.contains("destructive-dialog-open"));
}

#[test]
fn opening_any_dialog_moves_focus_and_blocks_all_library_keyboard_dispatch() {
    let library = library_source();
    let dialogs = dialogs_source();

    for contract in [
        "private property <bool> dialog-open: root.dialog.open",
        "changed dialog-open",
        "dialog-focus.focus()",
        "forward-focus: dialog-focus",
    ] {
        assert!(
            dialogs.contains(contract),
            "missing modal focus contract: {contract}"
        );
    }
    assert!(library.contains("if (root.dialog-open)"));
    let gate = library.find("if (root.dialog-open)").unwrap();
    let first_action = library.find("root.move-focus(-1)").unwrap();
    assert!(
        gate < first_action,
        "dialog gate must precede every library key action"
    );
}

#[test]
fn cards_and_context_menu_publish_focus_and_accessibility_state() {
    let source = library_source();
    for contract in [
        "accessible-role: list-item",
        "accessible-label: row.title + \". \" + row.subtitle",
        "accessible-description: row.warning",
        "accessible-value: row.duration",
        "accessible-checkable: root.selection-mode",
        "accessible-checked: row.selected",
        "accessible-expandable: true",
        "accessible-expanded: root.context-row == index",
        "border-color: root.focused-row == index",
        "focus-changed-event",
    ] {
        assert!(
            source.contains(contract),
            "missing accessibility/focus contract: {contract}"
        );
    }
}
