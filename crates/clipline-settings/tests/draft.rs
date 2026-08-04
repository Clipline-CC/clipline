use clipline_settings::{
    CloseRequest, CloseResult, CloudAccountDisplay, CloudWorkKind, CloudWorkOwner, DraftError,
    SettingsBackendDisplay, SettingsDraftController, SettingsField, SettingsPreferences,
    SettingsSessionGeneration, SettingsTab, TabNavigation,
};
use serde_json::Value;

fn preferences() -> SettingsPreferences {
    SettingsPreferences::from_document(&clipline_settings::AppSettings::default()).unwrap()
}

fn parity_fixture() -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/slint/settings-draft-parity.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn tab_order_navigation_and_roving_projection_are_pinned() {
    assert_eq!(
        SettingsTab::ALL,
        [
            SettingsTab::General,
            SettingsTab::Capture,
            SettingsTab::Recording,
            SettingsTab::Storage,
            SettingsTab::Hotkeys,
            SettingsTab::Games,
            SettingsTab::Cloud,
            SettingsTab::Support,
        ]
    );

    let mut draft = SettingsDraftController::open(None, preferences()).unwrap();
    draft.navigate(TabNavigation::Previous);
    assert_eq!(draft.active_tab(), SettingsTab::Support);
    draft.navigate(TabNavigation::Next);
    assert_eq!(draft.active_tab(), SettingsTab::General);
    draft.navigate(TabNavigation::End);
    assert_eq!(draft.active_tab(), SettingsTab::Support);
    draft.navigate(TabNavigation::Home);

    let tabs = draft.tab_projection().unwrap();
    assert_eq!(tabs.len(), 8);
    assert!(tabs[0].active && tabs[0].focused && tabs[0].tab_index == 0);
    assert!(tabs[1..]
        .iter()
        .all(|tab| !tab.active && !tab.focused && tab.tab_index == -1));
}

#[test]
fn exact_fields_and_tabs_are_dirty_and_edits_reset_the_warning() {
    let mut controller = SettingsDraftController::open(None, preferences()).unwrap();
    let mut changed = controller.draft().try_clone_bounded().unwrap();
    changed.capture_backend = clipline_settings::CaptureBackend::Wgc;
    changed.hotkey = "Ctrl+Shift+F9".into();
    controller.replace_draft(changed).unwrap();

    assert!(controller.is_dirty());
    assert!(controller.is_field_dirty(SettingsField::CaptureBackend));
    assert!(controller.is_field_dirty(SettingsField::PrimaryHotkey));
    assert!(controller.is_tab_dirty(SettingsTab::Capture));
    assert!(controller.is_tab_dirty(SettingsTab::Hotkeys));
    assert!(!controller.is_tab_dirty(SettingsTab::Cloud));

    let summary = controller.dirty_summary().unwrap();
    assert_eq!(
        summary.fields,
        vec![SettingsField::CaptureBackend, SettingsField::PrimaryHotkey]
    );
    assert_eq!(
        summary.tabs,
        vec![SettingsTab::Capture, SettingsTab::Hotkeys]
    );

    assert_eq!(
        controller.request_close(CloseRequest::Explicit).unwrap(),
        CloseResult::WarningArmed
    );
    assert!(controller.discard_warning_armed());
    let mut changed_again = controller.draft().try_clone_bounded().unwrap();
    changed_again.hotkey = "Alt+F11".into();
    controller.replace_draft(changed_again).unwrap();
    assert!(!controller.discard_warning_armed());
}

#[test]
fn close_escape_and_backdrop_preserve_the_two_step_contract() {
    for request in [CloseRequest::Explicit, CloseRequest::Escape] {
        let mut controller = SettingsDraftController::open(None, preferences()).unwrap();
        let mut changed = controller.draft().try_clone_bounded().unwrap();
        changed.open_on_startup = true;
        controller.replace_draft(changed).unwrap();

        assert_eq!(
            controller.request_close(request).unwrap(),
            CloseResult::WarningArmed
        );
        assert!(controller.is_dirty());
        assert_eq!(
            controller.request_close(request).unwrap(),
            CloseResult::DiscardedAndClose
        );
        assert!(!controller.is_dirty());
        assert_eq!(controller.draft(), controller.baseline());
    }

    let mut controller = SettingsDraftController::open(None, preferences()).unwrap();
    let mut changed = controller.draft().try_clone_bounded().unwrap();
    changed.close_to_tray = false;
    controller
        .replace_draft(changed.try_clone_bounded().unwrap())
        .unwrap();
    assert_eq!(
        controller.request_close(CloseRequest::Backdrop).unwrap(),
        CloseResult::WarningArmed
    );
    assert_eq!(
        controller.request_close(CloseRequest::Backdrop).unwrap(),
        CloseResult::WarningArmed
    );
    assert_eq!(controller.draft(), &changed);
    assert!(controller.is_dirty());

    let mut clean = SettingsDraftController::open(None, preferences()).unwrap();
    assert_eq!(
        clean.request_close(CloseRequest::Backdrop).unwrap(),
        CloseResult::CloseClean
    );
}

#[test]
fn support_hides_save_only_without_dirty_preferences() {
    let mut controller = SettingsDraftController::open(None, preferences()).unwrap();
    controller.activate_tab(SettingsTab::Support);
    assert!(!controller.save_visible());

    let mut changed = controller.draft().try_clone_bounded().unwrap();
    changed.cloud.delete_local_after_upload = true;
    controller.replace_draft(changed).unwrap();
    assert!(controller.save_visible());

    controller.activate_tab(SettingsTab::Capture);
    assert!(controller.save_visible());
}

#[test]
fn save_and_discard_rebase_while_stale_results_are_atomic() {
    let mut controller = SettingsDraftController::open(None, preferences()).unwrap();
    let mut first = controller.draft().try_clone_bounded().unwrap();
    first.replay_window_s = 90.0;
    controller
        .replace_draft(first.try_clone_bounded().unwrap())
        .unwrap();
    let stale = controller.begin_save().unwrap();

    let mut second = first;
    second.replay_window_s = 120.0;
    controller
        .replace_draft(second.try_clone_bounded().unwrap())
        .unwrap();
    let before = format!("{controller:?}");
    assert_eq!(
        controller
            .accept_saved(stale, second.try_clone_bounded().unwrap())
            .unwrap_err(),
        DraftError::StaleResult
    );
    assert_eq!(format!("{controller:?}"), before);

    let current = controller.begin_save().unwrap();
    controller
        .accept_saved(current, second.try_clone_bounded().unwrap())
        .unwrap();
    assert_eq!(controller.baseline(), &second);
    assert_eq!(controller.draft(), &second);
    assert!(!controller.is_dirty());

    let mut third = second.try_clone_bounded().unwrap();
    third.ui_theme = clipline_settings::UiTheme::Classic;
    controller.replace_draft(third).unwrap();
    controller.discard().unwrap();
    assert_eq!(controller.draft(), &second);
    assert!(!controller.is_dirty());
}

#[test]
fn checked_generations_fail_without_partial_mutation() {
    assert_eq!(
        SettingsDraftController::open(
            Some(SettingsSessionGeneration::new(u64::MAX)),
            preferences()
        )
        .unwrap_err(),
        DraftError::GenerationExhausted
    );

    let mut controller =
        SettingsDraftController::open_with_request_generation(None, u64::MAX, preferences())
            .unwrap();
    let mut changed = controller.draft().try_clone_bounded().unwrap();
    changed.minimize_to_tray = true;
    controller.replace_draft(changed).unwrap();
    let before = format!("{controller:?}");
    assert_eq!(
        controller
            .own_cloud_work(CloudWorkKind::ConnectDialog)
            .unwrap_err(),
        DraftError::GenerationExhausted
    );
    assert_eq!(format!("{controller:?}"), before);
    assert_eq!(
        controller.begin_save().unwrap_err(),
        DraftError::GenerationExhausted
    );
    assert_eq!(format!("{controller:?}"), before);
}

#[test]
fn same_owner_cloud_work_is_request_fenced_and_consumed_exactly_once() {
    let mut controller = SettingsDraftController::open(None, preferences()).unwrap();
    let older = controller
        .own_cloud_work(CloudWorkKind::ConnectDialog)
        .unwrap()
        .unwrap();
    let latest = controller
        .own_cloud_work(CloudWorkKind::ConnectDialog)
        .unwrap()
        .unwrap();
    assert!(latest.request_generation > older.request_generation);
    assert_eq!(
        controller.accept_cloud_work(&older).unwrap_err(),
        DraftError::StaleResult
    );
    assert_eq!(controller.owned_cloud_work(), Some(&latest));
    controller.accept_cloud_work(&latest).unwrap();
    assert!(controller.owned_cloud_work().is_none());
    assert_eq!(
        controller.accept_cloud_work(&latest).unwrap_err(),
        DraftError::StaleResult
    );

    controller
        .reconcile_backend(SettingsBackendDisplay {
            cloud_account: Some(CloudAccountDisplay::new("account-a", 1, "Dain").unwrap()),
            upload_count: 0,
            osu_connected: false,
        })
        .unwrap();
    let older_probe = controller
        .own_cloud_work(CloudWorkKind::Probe)
        .unwrap()
        .unwrap();
    let latest_probe = controller
        .own_cloud_work(CloudWorkKind::Probe)
        .unwrap()
        .unwrap();
    assert_eq!(
        controller.accept_cloud_work(&older_probe).unwrap_err(),
        DraftError::StaleResult
    );
    controller.accept_cloud_work(&latest_probe).unwrap();
}

#[test]
fn reopened_settings_session_rejects_old_work_for_the_same_account_generation() {
    let account = SettingsBackendDisplay {
        cloud_account: Some(CloudAccountDisplay::new("account-a", 7, "Dain").unwrap()),
        upload_count: 0,
        osu_connected: false,
    };
    let mut first = SettingsDraftController::open(None, preferences()).unwrap();
    first.reconcile_backend(account.clone()).unwrap();
    let stale = first.own_cloud_work(CloudWorkKind::Probe).unwrap().unwrap();

    let mut reopened = SettingsDraftController::open(Some(first.session()), preferences()).unwrap();
    reopened.reconcile_backend(account).unwrap();
    let current = reopened
        .own_cloud_work(CloudWorkKind::Probe)
        .unwrap()
        .unwrap();

    assert_eq!(stale.owner, current.owner);
    assert_eq!(stale.kind, current.kind);
    assert_eq!(stale.request_generation, current.request_generation);
    assert_ne!(stale.session, current.session);
    assert_eq!(
        reopened.accept_cloud_work(&stale).unwrap_err(),
        DraftError::StaleResult
    );
    reopened.accept_cloud_work(&current).unwrap();
}

#[test]
fn backend_reconciliation_never_changes_preferences_and_revokes_old_account_work() {
    let mut controller = SettingsDraftController::open(None, preferences()).unwrap();
    let mut changed = controller.draft().try_clone_bounded().unwrap();
    changed.cloud.auto_upload_rules = true;
    controller
        .replace_draft(changed.try_clone_bounded().unwrap())
        .unwrap();

    let disconnected_connect = controller
        .own_cloud_work(CloudWorkKind::ConnectDialog)
        .unwrap()
        .expect("a disconnected settings session must own its connect dialog");
    assert_eq!(disconnected_connect.kind, CloudWorkKind::ConnectDialog);
    assert_eq!(
        disconnected_connect.owner,
        CloudWorkOwner::Configuration(controller.cloud_configuration_owner())
    );

    let first = SettingsBackendDisplay {
        cloud_account: Some(CloudAccountDisplay::new("account-a", 7, "Dain").unwrap()),
        upload_count: 2,
        osu_connected: true,
    };
    controller.reconcile_backend(first.clone()).unwrap();
    assert!(controller.owned_cloud_work().is_none());
    let owned = controller
        .own_cloud_work(CloudWorkKind::ConnectDialog)
        .unwrap();
    let owned = owned.unwrap();
    assert_eq!(
        owned.owner,
        CloudWorkOwner::Configuration(controller.cloud_configuration_owner())
    );

    let probe = controller
        .own_cloud_work(CloudWorkKind::Probe)
        .unwrap()
        .unwrap();
    assert_eq!(
        probe.owner,
        CloudWorkOwner::Account(first.cloud_account.as_ref().unwrap().owner().clone())
    );

    let replacement = SettingsBackendDisplay {
        cloud_account: Some(CloudAccountDisplay::new("account-b", 8, "Replacement").unwrap()),
        upload_count: 0,
        osu_connected: false,
    };
    controller.reconcile_backend(replacement.clone()).unwrap();

    assert_eq!(controller.draft(), &changed);
    assert!(controller.is_field_dirty(SettingsField::CloudAutoUpload));
    assert_eq!(controller.backend_display(), &replacement);
    assert!(controller.owned_cloud_work().is_none());
}

#[test]
fn rust_controller_matches_every_frozen_webview_parity_vector() {
    let fixture = parity_fixture();
    assert_eq!(fixture["schemaVersion"], 1);
    let expected_tabs = fixture["tabOrder"]
        .as_array()
        .unwrap()
        .iter()
        .map(Value::as_str)
        .collect::<Option<Vec<_>>>()
        .unwrap();
    assert_eq!(
        expected_tabs,
        vec![
            "general",
            "capture",
            "recording",
            "storage",
            "hotkeys",
            "games",
            "cloud",
            "support",
        ]
    );

    for vector in fixture["dirtyVectors"].as_array().unwrap() {
        let baseline_value = &vector["baseline"];
        let draft_value = &vector["draft"];
        let mut baseline = preferences();
        apply_parity_value(&mut baseline, baseline_value);
        let mut draft = baseline.try_clone_bounded().unwrap();
        apply_parity_value(&mut draft, draft_value);
        // Upload status is deliberately absent from SettingsPreferences. Its
        // fixture values are consumed by the retained JavaScript oracle.
        assert!(baseline_value["uploadStatus"].is_string());
        assert!(draft_value["uploadStatus"].is_string());

        let mut controller = SettingsDraftController::open(None, baseline).unwrap();
        controller.replace_draft(draft).unwrap();
        assert_eq!(
            controller.is_dirty(),
            vector["expectedDirty"].as_bool().unwrap(),
            "{}",
            vector["name"]
        );
    }

    for vector in fixture["closeVectors"].as_array().unwrap() {
        let mut controller = SettingsDraftController::open(None, preferences()).unwrap();
        if vector["dirty"].as_bool().unwrap() {
            let mut changed = controller.draft().try_clone_bounded().unwrap();
            changed.open_on_startup = !changed.open_on_startup;
            controller.replace_draft(changed).unwrap();
        }
        if vector["warningArmed"].as_bool().unwrap() {
            assert_eq!(
                controller.request_close(CloseRequest::Explicit).unwrap(),
                CloseResult::WarningArmed
            );
        }
        let request = if vector["allowDiscard"].as_bool().unwrap() {
            CloseRequest::Explicit
        } else {
            CloseRequest::Backdrop
        };
        let result = controller.request_close(request).unwrap();
        let actual = match result {
            CloseResult::CloseClean => "close",
            CloseResult::WarningArmed => "warn",
            CloseResult::DiscardedAndClose => "discard",
        };
        assert_eq!(
            actual,
            vector["expected"].as_str().unwrap(),
            "{}",
            vector["name"]
        );
    }
}

fn apply_parity_value(preferences: &mut SettingsPreferences, value: &Value) {
    preferences.open_on_startup = value["openOnStartup"].as_bool().unwrap();
    preferences.close_to_tray = value["closeToTray"].as_bool().unwrap();
    preferences.capture_backend = match value["captureBackend"].as_str().unwrap() {
        "auto" => clipline_settings::CaptureBackend::Auto,
        "wgc" => clipline_settings::CaptureBackend::Wgc,
        other => panic!("unknown capture backend {other}"),
    };
    preferences.capture_region.x = i32::try_from(value["captureX"].as_i64().unwrap()).unwrap();
    preferences.audio.mic_enabled = value["micEnabled"].as_bool().unwrap();
    preferences.replay_window_s = value["replayWindow"].as_f64().unwrap();
    preferences.disk_quota_gb = value["diskQuota"].as_f64().unwrap();
    preferences.hotkey = value["hotkey"].as_str().unwrap().into();
    preferences.games.auto_detect = value["gamesAutoDetect"].as_bool().unwrap();
    preferences.cloud.default_visibility = value["visibility"].as_str().unwrap().into();
}
