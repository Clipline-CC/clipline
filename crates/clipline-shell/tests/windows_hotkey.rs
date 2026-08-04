#![cfg(windows)]

use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use clipline_shell::hotkey::{parse_hotkey_spec, HotkeySet};
use clipline_shell::windows::hotkey::WindowsHotkeyService;
use clipline_shell::{shell_command_channel, ShellCommand};

fn device_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .expect("hotkey device test lock")
}

#[test]
fn service_registers_replaces_dispatches_and_joins() {
    let _guard = device_test_lock();
    let (sender, receiver) = shell_command_channel();
    let service = WindowsHotkeyService::start(sender).expect("start Windows hotkey service");
    let hotkeys = HotkeySet::parse(&["Ctrl+Alt+Shift+F23"]).unwrap();
    let outcome = service.replace(&hotkeys).expect("register test hotkey");
    assert!(outcome.warnings.is_empty());
    assert_eq!(service.snapshot().active_labels, ["Ctrl+Alt+Shift+F23"]);

    let hotkey = parse_hotkey_spec("Ctrl+Alt+Shift+F23").unwrap();
    service
        .post_registered_for_test(&hotkey)
        .expect("post registered notification");
    let update = receiver
        .wait_recv(Duration::from_secs(2))
        .expect("command producer connected")
        .expect("save command delivered");
    assert_eq!(update.command, ShellCommand::SaveReplay);

    service
        .replace(&HotkeySet::parse(&[]).unwrap())
        .expect("unregister test hotkey");
    assert!(service.snapshot().active_labels.is_empty());
    service.shutdown().expect("stop and join hotkey service");
}

#[test]
fn mouse_hook_is_owned_only_while_a_mouse_chord_is_active() {
    let _guard = device_test_lock();
    let (sender, _receiver) = shell_command_channel();
    let service = WindowsHotkeyService::start(sender).expect("start Windows hotkey service");
    service
        .replace(&HotkeySet::parse(&["Ctrl+Mouse5"]).unwrap())
        .expect("install mouse chord");
    assert!(service.snapshot().mouse_hook_installed);
    service
        .replace(&HotkeySet::parse(&["Ctrl+Alt+Shift+F22"]).unwrap())
        .expect("replace mouse chord");
    assert!(!service.snapshot().mouse_hook_installed);
    service.shutdown().expect("stop and join hotkey service");
}

#[test]
fn rollback_receipt_restores_exact_state_and_rejects_a_newer_owner() {
    let _guard = device_test_lock();
    let (sender, _receiver) = shell_command_channel();
    let service = WindowsHotkeyService::start(sender).expect("start Windows hotkey service");
    let first = HotkeySet::parse(&["Ctrl+Alt+Shift+F21"]).unwrap();
    let second = HotkeySet::parse(&["Ctrl+Alt+Shift+F22"]).unwrap();

    let first_change = service
        .replace_with_receipt(&first)
        .expect("register first test hotkey");
    service
        .rollback(first_change.receipt)
        .expect("restore empty starting set");
    assert!(service.snapshot().active_labels.is_empty());

    let stale_change = service
        .replace_with_receipt(&first)
        .expect("register first test hotkey again");
    service
        .replace(&second)
        .expect("publish newer hotkey owner");
    let error = service
        .rollback(stale_change.receipt)
        .expect_err("stale receipt must not overwrite newer owner");
    assert!(error.to_string().contains("changed concurrently"));
    assert_eq!(service.snapshot().active_labels, ["Ctrl+Alt+Shift+F22"]);

    service
        .replace(&HotkeySet::parse(&[]).unwrap())
        .expect("unregister test hotkey");
    service.shutdown().expect("stop and join hotkey service");
}
