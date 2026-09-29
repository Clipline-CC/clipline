
use tauri::{
    AppHandle, Manager, Runtime,
};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut};


use crate::settings::{
    is_global_shortcut_hotkey, parse_hotkey, AppSettings,
};
use super::*;

/// Brings the OS registrations in line with the configured global shortcuts:
/// registers shortcuts new in `new`, unregisters ones dropped from `old`.
/// A registration failure for a shortcut that was already configured (a
/// retry of one that was unavailable earlier) is returned as a warning; a
/// failure for a newly added or removed shortcut rolls back this call's
/// registrations and aborts.
pub(crate) fn sync_global_hotkeys<E>(
    old: &[Shortcut],
    new: &[Shortcut],
    is_registered: impl Fn(Shortcut) -> bool,
    mut register: impl FnMut(Shortcut) -> Result<(), E>,
    mut unregister: impl FnMut(Shortcut) -> Result<(), E>,
) -> Result<Vec<String>, String>
where
    E: std::fmt::Display,
{
    let mut warnings = Vec::new();
    let mut added = Vec::new();
    for shortcut in new {
        if is_registered(*shortcut) {
            continue;
        }
        match register(*shortcut) {
            Ok(()) => added.push(*shortcut),
            Err(e) if old.contains(shortcut) => {
                warnings.push(format!("global save hotkey still unavailable: {e}"));
            }
            Err(e) => {
                for shortcut in added {
                    let _ = unregister(shortcut);
                }
                return Err(format!("register hotkey: {e}"));
            }
        }
    }
    let mut removed = Vec::new();
    for shortcut in old {
        if new.contains(shortcut) || !is_registered(*shortcut) {
            continue;
        }
        if let Err(e) = unregister(*shortcut) {
            let mut rollback_errors = Vec::new();
            for shortcut in removed.into_iter().rev() {
                if let Err(rollback) = register(shortcut) {
                    rollback_errors.push(format!("re-register {shortcut}: {rollback}"));
                }
            }
            for shortcut in added {
                if let Err(rollback) = unregister(shortcut) {
                    rollback_errors.push(format!("unregister {shortcut}: {rollback}"));
                }
            }
            let mut message = format!("replace hotkey: {e}");
            if !rollback_errors.is_empty() {
                message.push_str(&format!(
                    "; rollback incomplete: {}",
                    rollback_errors.join(", ")
                ));
            }
            return Err(message);
        }
        removed.push(*shortcut);
    }
    Ok(warnings)
}

pub(crate) fn parse_global_hotkey(raw: &str) -> Result<Option<Shortcut>, String> {
    if is_global_shortcut_hotkey(raw)? {
        parse_hotkey(raw).map(Some)
    } else {
        Ok(None)
    }
}

/// The configured Save Replay keybinds that go through the OS global-shortcut
/// registry (mouse and modified keyboard binds use the low-level hook instead).
pub(crate) fn global_hotkeys(settings: &AppSettings) -> Result<Vec<Shortcut>, String> {
    let mut shortcuts = Vec::new();
    for raw in settings.hotkeys() {
        if let Some(shortcut) = parse_global_hotkey(raw)? {
            shortcuts.push(shortcut);
        }
    }
    Ok(shortcuts)
}

/// What the OS should own right now. Capture unregisters every global shortcut
/// so the key being bound can reach the field instead of saving a replay.
pub(crate) fn effective_global_hotkeys(
    settings: &AppSettings,
    capture_active: bool,
) -> Result<Vec<Shortcut>, String> {
    if capture_active {
        Ok(Vec::new())
    } else {
        global_hotkeys(settings)
    }
}

pub(crate) fn apply_hotkey_capture_active<R: Runtime>(
    app: &AppHandle<R>,
    state: &RuntimeState,
    active: bool,
) -> Result<(), String> {
    let settings = state.settings();
    let previous = crate::hotkeys::actions_paused();
    let old = effective_global_hotkeys(&settings, previous)?;
    let new = effective_global_hotkeys(&settings, active)?;
    let warnings = commit_hotkey_capture_pause(active, || {
        let shortcuts = app.global_shortcut();
        sync_global_hotkeys(
            &old,
            &new,
            |shortcut| shortcuts.is_registered(shortcut),
            |shortcut| shortcuts.register(shortcut),
            |shortcut| shortcuts.unregister(shortcut),
        )
    })?;
    for warning in warnings {
        tracing::warn!(event = "hotkey_capture_global_sync_warning", message = %warning);
    }
    Ok(())
}

pub(crate) fn commit_hotkey_capture_pause<T>(
    active: bool,
    sync: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    let previous = crate::hotkeys::actions_paused();
    crate::hotkeys::set_actions_paused(active);
    match sync() {
        Ok(value) => Ok(value),
        Err(error) => {
            crate::hotkeys::set_actions_paused(previous);
            Err(error)
        }
    }
}

pub(crate) fn resume_hotkeys_after_ui_gone<R: Runtime>(app: &AppHandle<R>) {
    if !crate::hotkeys::actions_paused() {
        return;
    }
    if let Err(error) = apply_hotkey_capture_active(app, &app.state::<RuntimeState>(), false) {
        tracing::warn!(event = "hotkey_capture_resume_failed", error = %error);
        log_diagnostic(format!("resume hotkeys after UI gone: {error}"));
    }
}

#[tauri::command]
pub(crate) fn set_hotkey_capture_active<R: Runtime>(
    app: AppHandle<R>,
    state: tauri::State<RuntimeState>,
    active: bool,
) -> Result<(), String> {
    apply_hotkey_capture_active(&app, &state, active)
}

pub(crate) fn save_hotkey_label(settings: &AppSettings) -> String {
    settings.hotkeys().join(" / ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_global_hotkeys_skips_unregister_when_old_shortcut_is_stale() {
        let old_shortcut = parse_hotkey("Alt+F10").unwrap();
        let new_shortcut = parse_hotkey("Ctrl+F8").unwrap();
        let mut registered = Vec::new();
        let mut unregistered = Vec::new();

        let result = sync_global_hotkeys(
            &[old_shortcut],
            &[new_shortcut],
            |_| false,
            |shortcut| {
                registered.push(shortcut);
                Ok::<_, &'static str>(())
            },
            |shortcut| {
                unregistered.push(shortcut);
                Err::<(), _>("old shortcut was never registered")
            },
        );

        assert_eq!(result, Ok(Vec::new()));
        assert_eq!(registered, vec![new_shortcut]);
        assert!(unregistered.is_empty());
    }

    #[test]
    fn capturing_a_hotkey_unbinds_os_global_shortcuts() {
        let settings = AppSettings::default();
        let live = global_hotkeys(&settings).unwrap();
        assert!(
            live.contains(&parse_hotkey("F6").unwrap()),
            "the default Save Replay bind must be an OS global shortcut"
        );
        assert!(
            effective_global_hotkeys(&settings, true)
                .unwrap()
                .is_empty(),
            "capture must drop RegisterHotKey so the field can see the live save key"
        );
        assert_eq!(effective_global_hotkeys(&settings, false).unwrap(), live);
    }

    #[test]
    fn failed_hotkey_capture_sync_restores_the_previous_pause_flag() {
        crate::hotkeys::set_actions_paused(false);
        let paused: Result<(), String> =
            commit_hotkey_capture_pause(true, || Err("unregister failed".into()));
        assert_eq!(paused, Err("unregister failed".into()));
        assert!(
            !crate::hotkeys::actions_paused(),
            "a failed pause must not leave live actions suppressed"
        );

        crate::hotkeys::set_actions_paused(true);
        let resumed: Result<(), String> =
            commit_hotkey_capture_pause(false, || Err("register failed".into()));
        assert_eq!(resumed, Err("register failed".into()));
        assert!(
            crate::hotkeys::actions_paused(),
            "a failed resume must keep the pause the UI still believes is active"
        );
        crate::hotkeys::set_actions_paused(false);
    }

    #[test]
    fn missing_unchanged_global_hotkey_is_retried_without_blocking_save() {
        let shortcut = parse_hotkey("Alt+F10").unwrap();
        let mut registered = Vec::new();

        let result = sync_global_hotkeys(
            &[shortcut],
            &[shortcut],
            |_| false,
            |shortcut| {
                registered.push(shortcut);
                Err::<(), _>("still owned by another app")
            },
            |_| Ok(()),
        );

        let warnings = result.expect("retry failure must not block save");
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("still owned by another app"));
        assert_eq!(registered, vec![shortcut]);
    }

    #[test]
    fn sync_global_hotkeys_adds_secondary_and_keeps_registered_primary() {
        let primary = parse_hotkey("Alt+F10").unwrap();
        let secondary = parse_hotkey("Ctrl+F8").unwrap();
        let mut registered = Vec::new();
        let mut unregistered = Vec::new();

        let result = sync_global_hotkeys(
            &[primary],
            &[primary, secondary],
            |shortcut| shortcut == primary,
            |shortcut| {
                registered.push(shortcut);
                Ok::<_, &'static str>(())
            },
            |shortcut| {
                unregistered.push(shortcut);
                Ok(())
            },
        );

        assert_eq!(result, Ok(Vec::new()));
        assert_eq!(registered, vec![secondary]);
        assert!(unregistered.is_empty());
    }

    #[test]
    fn sync_global_hotkeys_rolls_back_new_registrations_on_failure() {
        let secondary = parse_hotkey("Ctrl+F8").unwrap();
        let removed = parse_hotkey("Alt+F10").unwrap();
        let mut unregistered = Vec::new();

        let result = sync_global_hotkeys(
            &[removed],
            &[secondary],
            |shortcut| shortcut == removed,
            |_| Ok::<_, &'static str>(()),
            |shortcut| {
                unregistered.push(shortcut);
                if shortcut == removed {
                    Err("cannot unregister")
                } else {
                    Ok(())
                }
            },
        );

        assert!(result.is_err());
        assert_eq!(unregistered, vec![removed, secondary]);
    }

    #[test]
    fn sync_global_hotkeys_restores_earlier_removals_when_a_later_one_fails() {
        let first = parse_hotkey("Alt+F10").unwrap();
        let second = parse_hotkey("Ctrl+F8").unwrap();
        let mut registered = Vec::new();
        let mut unregistered = Vec::new();

        let result = sync_global_hotkeys(
            &[first, second],
            &[],
            |_| true,
            |shortcut| {
                registered.push(shortcut);
                Ok::<_, &'static str>(())
            },
            |shortcut| {
                unregistered.push(shortcut);
                if shortcut == second {
                    Err("second removal failed")
                } else {
                    Ok(())
                }
            },
        );

        assert!(result.is_err());
        assert_eq!(unregistered, vec![first, second]);
        assert_eq!(registered, vec![first]);
    }

    #[test]
    fn sync_global_hotkeys_surfaces_rollback_failures() {
        let first = parse_hotkey("Alt+F10").unwrap();
        let second = parse_hotkey("Ctrl+F8").unwrap();

        let error = sync_global_hotkeys(
            &[first, second],
            &[],
            |_| true,
            |_| Err::<(), _>("restore failed"),
            |shortcut| {
                if shortcut == second {
                    Err("second removal failed")
                } else {
                    Ok(())
                }
            },
        )
        .unwrap_err();

        assert!(error.contains("rollback incomplete"), "{error}");
        assert!(error.contains("restore failed"), "{error}");
    }

    #[test]
    fn sync_global_hotkeys_removes_dropped_secondary() {
        let primary = parse_hotkey("Alt+F10").unwrap();
        let secondary = parse_hotkey("Ctrl+F8").unwrap();
        let mut registered = Vec::new();
        let mut unregistered = Vec::new();

        let result = sync_global_hotkeys(
            &[primary, secondary],
            &[primary],
            |_| true,
            |shortcut| {
                registered.push(shortcut);
                Ok::<_, &'static str>(())
            },
            |shortcut| {
                unregistered.push(shortcut);
                Ok(())
            },
        );

        assert_eq!(result, Ok(Vec::new()));
        assert!(registered.is_empty());
        assert_eq!(unregistered, vec![secondary]);
    }
}
