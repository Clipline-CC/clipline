
use tauri::{
    AppHandle, Emitter, Runtime,
};
#[cfg(test)]
use std::sync::MutexGuard;
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut};


use crate::service::{self};
use crate::settings::{
    quota_bytes_from_gb, AppSettings,
};
use super::*;

#[cfg(test)]
pub(crate) fn run_before_releasing_settings_save_lock<T>(
    save_guard: MutexGuard<'_, ()>,
    operation: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    let result = operation();
    drop(save_guard);
    result
}

#[derive(Default)]
pub(crate) struct AppliedSettingsSideEffects {
    pub(crate) global_hotkeys: bool,
    pub(crate) hook_hotkeys: bool,
    pub(crate) tray_label: bool,
    pub(crate) autostart: bool,
}

pub(crate) fn rollback_settings_side_effects<R: Runtime>(
    app: &AppHandle<R>,
    tray_items: &TrayItems<R>,
    old: &AppSettings,
    old_global_hotkeys: &[Shortcut],
    new_global_hotkeys: &[Shortcut],
    applied: &AppliedSettingsSideEffects,
) -> Vec<String> {
    let mut errors = Vec::new();
    if applied.autostart {
        if let Err(error) = set_autostart(app, old.open_on_startup) {
            errors.push(format!("restore Windows startup registration: {error}"));
        }
    }
    if applied.tray_label {
        if let Err(error) = tray_items.set_hotkey_label(&save_hotkey_label(old)) {
            errors.push(format!("restore tray hotkey label: {error}"));
        }
    }
    if applied.hook_hotkeys {
        let save = old.hotkeys();
        let recording = old.recording_hotkeys();
        let bookmark = old.bookmark_hotkeys();
        if let Err(error) = crate::hotkeys::set_hotkeys(crate::hotkeys::HookHotkeys {
            save: &save,
            recording: &recording,
            bookmark: &bookmark,
        }) {
            errors.push(format!("restore low-level hotkeys: {error}"));
        }
    }
    if applied.global_hotkeys {
        let shortcuts = app.global_shortcut();
        if let Err(error) = sync_global_hotkeys(
            new_global_hotkeys,
            old_global_hotkeys,
            |shortcut| shortcuts.is_registered(shortcut),
            |shortcut| shortcuts.register(shortcut),
            |shortcut| shortcuts.unregister(shortcut),
        ) {
            errors.push(format!("restore global save hotkeys: {error}"));
        }
    }
    errors
}

pub(crate) fn settings_transaction_error(primary: String, rollback_errors: Vec<String>) -> String {
    if rollback_errors.is_empty() {
        primary
    } else {
        format!(
            "{primary}; settings rollback incomplete: {}",
            rollback_errors.join(", ")
        )
    }
}

#[tauri::command]
pub(crate) fn save_settings<R: Runtime>(
    app: AppHandle<R>,
    state: tauri::State<RuntimeState>,
    first_run_state: tauri::State<FirstRunState>,
    tray_items: tauri::State<TrayItems<R>>,
    storage_settings: tauri::State<crate::library::StorageSettings>,
    media_folder_authorization: tauri::State<NativeMediaFolderAuthorization>,
    mut settings: AppSettings,
) -> Result<AppSettings, String> {
    settings.hotkey = crate::settings::normalize_hotkey(&settings.hotkey)?;
    settings.hotkey_secondary = match settings.hotkey_secondary.as_deref() {
        Some(raw) if !raw.trim().is_empty() => Some(crate::settings::normalize_hotkey(raw)?),
        _ => None,
    };
    settings.recording_hotkey = match settings.recording_hotkey.as_deref() {
        Some(raw) if !raw.trim().is_empty() => Some(crate::settings::normalize_hotkey(raw)?),
        _ => None,
    };
    settings.recording_hotkey_secondary = match settings.recording_hotkey_secondary.as_deref() {
        Some(raw) if !raw.trim().is_empty() => Some(crate::settings::normalize_hotkey(raw)?),
        _ => None,
    };
    settings.games.normalize();
    settings.validate()?;
    let media_dir = settings.media_dir_path()?;
    let cloud_save_guard = RuntimeState::lock_cloud_settings_save()?;
    let old = state.settings();
    preserve_backend_owned_settings_fields(&mut settings, &old);
    let old_media_dir = old.media_dir_path()?;
    media_folder_authorization.validate_change(&old_media_dir, &media_dir)?;
    service::prepare_writable_media_directory(&media_dir)?;

    // Apply the autostart registry change before persisting so settings.json
    // can never say "enabled" while the Run key update failed. Debug builds
    // share settings with installed builds, so they preserve this preference
    // and leave the shared Run key alone.
    let requested_open_on_startup = settings.open_on_startup;
    settings.open_on_startup = saved_autostart_preference_for_current_build(
        requested_open_on_startup,
        old.open_on_startup,
    );
    let old_global_hotkeys = effective_global_hotkeys(&old, crate::hotkeys::actions_paused())?;
    let new_global_hotkeys = effective_global_hotkeys(&settings, crate::hotkeys::actions_paused())?;
    let quota_bytes = quota_bytes_from_gb(settings.disk_quota_gb)?;
    let shortcuts = app.global_shortcut();
    let mut applied = AppliedSettingsSideEffects::default();
    let warnings = sync_global_hotkeys(
        &old_global_hotkeys,
        &new_global_hotkeys,
        |shortcut| shortcuts.is_registered(shortcut),
        |shortcut| shortcuts.register(shortcut),
        |shortcut| shortcuts.unregister(shortcut),
    )?;
    applied.global_hotkeys = true;
    let new_save_hotkeys = settings.hotkeys();
    let new_recording_hotkeys = settings.recording_hotkeys();
    let new_bookmark_hotkeys = settings.bookmark_hotkeys();
    if let Err(primary) = crate::hotkeys::set_hotkeys(crate::hotkeys::HookHotkeys {
        save: &new_save_hotkeys,
        recording: &new_recording_hotkeys,
        bookmark: &new_bookmark_hotkeys,
    }) {
        let rollback = rollback_settings_side_effects(
            &app,
            &tray_items,
            &old,
            &old_global_hotkeys,
            &new_global_hotkeys,
            &applied,
        );
        return Err(settings_transaction_error(primary, rollback));
    }
    applied.hook_hotkeys = true;
    if let Err(primary) = tray_items.set_hotkey_label(&save_hotkey_label(&settings)) {
        let rollback = rollback_settings_side_effects(
            &app,
            &tray_items,
            &old,
            &old_global_hotkeys,
            &new_global_hotkeys,
            &applied,
        );
        return Err(settings_transaction_error(primary, rollback));
    }
    applied.tray_label = true;
    if settings.open_on_startup != old.open_on_startup
        && autostart_should_mutate_for_current_build()
    {
        match set_autostart(&app, settings.open_on_startup) {
            Ok(actual) => {
                settings.open_on_startup = actual;
                applied.autostart = true;
            }
            Err(primary) => {
                let rollback = rollback_settings_side_effects(
                    &app,
                    &tray_items,
                    &old,
                    &old_global_hotkeys,
                    &new_global_hotkeys,
                    &applied,
                );
                return Err(settings_transaction_error(
                    format!("update Windows startup registration: {primary}"),
                    rollback,
                ));
            }
        }
    }
    let prepared_restart = match state.prepare_settings_restart(settings.clone()) {
        Ok(prepared) => prepared,
        Err(primary) => {
            let rollback = rollback_settings_side_effects(
                &app,
                &tray_items,
                &old,
                &old_global_hotkeys,
                &new_global_hotkeys,
                &applied,
            );
            return Err(settings_transaction_error(primary, rollback));
        }
    };
    if let Err(error) = settings.save() {
        let rollback = rollback_settings_side_effects(
            &app,
            &tray_items,
            &old,
            &old_global_hotkeys,
            &new_global_hotkeys,
            &applied,
        );
        return Err(settings_transaction_error(error, rollback));
    }
    if let Err(primary) = state.finish_prepared_restart(app.clone(), prepared_restart) {
        let mut rollback = Vec::new();
        if let Err(error) = old.save() {
            rollback.push(format!("restore settings.json: {error}"));
        }
        rollback.extend(rollback_settings_side_effects(
            &app,
            &tray_items,
            &old,
            &old_global_hotkeys,
            &new_global_hotkeys,
            &applied,
        ));
        return Err(settings_transaction_error(primary, rollback));
    }
    drop(cloud_save_guard);
    for message in warnings {
        tracing::warn!(event = "settings_apply_warning", message = %message);
        let _ = app.emit("error", message);
    }
    storage_settings.set_quota_bytes(quota_bytes);
    storage_settings.set_media_dir(media_dir.clone());
    if let Err(error) = state.recheck_storage_quota(
        app.clone(),
        &media_dir,
        quota_bytes,
        settings.auto_delete_when_over_quota,
        true,
    ) {
        tracing::warn!(event = "storage_quota_recheck_failed", error = %error);
        let _ = app.emit("error", error);
    }
    media_folder_authorization.commit(&media_dir);
    first_run_state.complete();
    Ok(settings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_transaction_error_preserves_primary_and_rollback_failures() {
        assert_eq!(
            settings_transaction_error("save failed".into(), Vec::new()),
            "save failed"
        );
        let error = settings_transaction_error(
            "save failed".into(),
            vec![
                "restore autostart failed".into(),
                "restore hotkey failed".into(),
            ],
        );
        assert!(error.starts_with("save failed"), "{error}");
        assert!(error.contains("restore autostart failed"), "{error}");
        assert!(error.contains("restore hotkey failed"), "{error}");
    }
}
