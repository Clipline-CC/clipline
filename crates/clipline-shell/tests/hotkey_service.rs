use std::collections::BTreeSet;

use clipline_shell::hotkey::{
    replace_hotkeys, HotkeyRegistrationBackend, HotkeySet, HotkeySetError, HotkeySpec,
    HotkeyTriggerGate, MAX_CONFIGURED_HOTKEYS,
};

#[derive(Default)]
struct FakeBackend {
    registered: BTreeSet<String>,
    fail_register: BTreeSet<String>,
    fail_unregister: BTreeSet<String>,
    operations: Vec<String>,
}

impl HotkeyRegistrationBackend for FakeBackend {
    type Error = &'static str;

    fn is_registered(&self, hotkey: &HotkeySpec) -> bool {
        self.registered.contains(&hotkey.normalized())
    }

    fn register(&mut self, hotkey: &HotkeySpec) -> Result<(), Self::Error> {
        let label = hotkey.normalized();
        self.operations.push(format!("register:{label}"));
        if self.fail_register.contains(&label) {
            return Err("register failed");
        }
        self.registered.insert(label);
        Ok(())
    }

    fn unregister(&mut self, hotkey: &HotkeySpec) -> Result<(), Self::Error> {
        let label = hotkey.normalized();
        self.operations.push(format!("unregister:{label}"));
        if self.fail_unregister.contains(&label) {
            return Err("unregister failed");
        }
        self.registered.remove(&label);
        Ok(())
    }
}

#[test]
fn configured_set_is_bounded_distinct_and_atomic() {
    let set = HotkeySet::parse(&["Alt+F10", "Ctrl+Mouse5"]).unwrap();
    assert_eq!(set.labels(), ["Alt+F10", "Ctrl+Mouse5"]);
    assert_eq!(set.len(), MAX_CONFIGURED_HOTKEYS);

    assert!(matches!(
        HotkeySet::parse(&["Alt+F10", "alt+f10"]),
        Err(HotkeySetError::Duplicate { .. })
    ));
    assert!(matches!(
        HotkeySet::parse(&["F1", "F2", "F3"]),
        Err(HotkeySetError::TooMany { .. })
    ));
    assert!(matches!(
        HotkeySet::parse(&["Alt+F10", "F12"]),
        Err(HotkeySetError::Invalid { index: 1, .. })
    ));
}

#[test]
fn replacement_rolls_back_new_registrations_on_failure() {
    let old = HotkeySet::parse(&["Alt+F10"]).unwrap();
    let new = HotkeySet::parse(&["Ctrl+F8", "Shift+F9"]).unwrap();
    let mut backend = FakeBackend::default();
    backend.registered.insert("Alt+F10".into());
    backend.fail_register.insert("Shift+F9".into());

    let error = replace_hotkeys(&old, &new, &mut backend).unwrap_err();
    assert!(error.to_string().contains("register hotkey Shift+F9"));
    assert_eq!(backend.registered, BTreeSet::from(["Alt+F10".into()]));
    assert_eq!(
        backend.operations,
        [
            "register:Ctrl+F8",
            "register:Shift+F9",
            "unregister:Ctrl+F8",
        ]
    );
}

#[test]
fn replacement_restores_removed_and_added_entries_when_removal_fails() {
    let old = HotkeySet::parse(&["Alt+F10", "Ctrl+F8"]).unwrap();
    let new = HotkeySet::parse(&["Shift+F9"]).unwrap();
    let mut backend = FakeBackend {
        registered: BTreeSet::from(["Alt+F10".into(), "Ctrl+F8".into()]),
        ..FakeBackend::default()
    };
    backend.fail_unregister.insert("Ctrl+F8".into());

    let error = replace_hotkeys(&old, &new, &mut backend).unwrap_err();
    assert!(error.to_string().contains("unregister hotkey Ctrl+F8"));
    assert_eq!(
        backend.registered,
        BTreeSet::from(["Alt+F10".into(), "Ctrl+F8".into()])
    );
    assert_eq!(
        backend.operations,
        [
            "register:Shift+F9",
            "unregister:Alt+F10",
            "unregister:Ctrl+F8",
            "register:Alt+F10",
            "unregister:Shift+F9",
        ]
    );
}

#[test]
fn unchanged_unavailable_hotkey_preserves_legacy_warning_text() {
    let old = HotkeySet::parse(&["Alt+F10"]).unwrap();
    let new = old.clone();
    let mut backend = FakeBackend::default();
    backend.fail_register.insert("Alt+F10".into());

    let outcome = replace_hotkeys(&old, &new, &mut backend).unwrap();
    assert_eq!(outcome.warnings.len(), 1);
    assert_eq!(
        outcome.warnings[0],
        "global save hotkey still unavailable: register failed"
    );
}

#[test]
fn trigger_gate_delivers_once_per_key_down_and_deduplicates_registered_fallback_paths() {
    let mut gate = HotkeyTriggerGate::new(150);
    assert!(gate.observe_registered(0x79, 1_000));
    assert!(!gate.observe_hook_key_down(0x79, 1_001));
    assert!(!gate.observe_hook_key_down(0x79, 1_010));
    gate.observe_key_up(0x79);
    assert!(gate.observe_hook_key_down(0x79, 1_200));
    assert!(!gate.observe_registered(0x79, 1_201));
    gate.observe_key_up(0x79);
    assert!(gate.observe_registered(0x79, 1_400));
}

#[test]
fn ineligible_hook_observation_does_not_suppress_registered_delivery() {
    let mut gate = HotkeyTriggerGate::new(150);
    assert!(!gate.observe_hook_key_down_if(0x79, 1_000, false));
    assert!(gate.observe_registered(0x79, 1_001));
    gate.observe_key_up(0x79);
}
