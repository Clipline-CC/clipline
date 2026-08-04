use std::collections::BTreeMap;

use clipline_settings::{
    AdvancedRecordingSettings, AppSettings, AudioChannelMode, CaptureBackend, CaptureMode,
    CaptureRegionSettings, CloudUploadPreferences, CloudUploadRecord, CustomGameSettings,
    GamePluginPreference, GamePluginSettings, GameRecordingMode, GameSettings, OsuApiSettings,
    OutputResolution, ReplayStorageMode, SettingsPreferences, UiTheme, UpdateChannel, VideoEncoder,
    VideoQuality, MAX_SETTINGS_COLLECTION_BYTES, MAX_SETTINGS_CUSTOM_GAMES,
    MAX_SETTINGS_FIELD_BYTES, MAX_SETTINGS_GAME_PLUGINS,
};

fn backend_populated_document() -> AppSettings {
    let mut document = AppSettings::default();
    document.cloud.host_url = "https://cloud.example/api/".into();
    document.cloud.public_url = Some("https://clips.example/u/test".into());
    document.cloud.connected_user_id = Some("user-17".into());
    document.cloud.connected_username = Some("tester".into());
    document.cloud.connected_display_name = Some("Test User".into());
    document.cloud.credential_target = Some("Clipline Cloud:test".into());
    document.cloud.credential_cleanup_targets = vec![
        "Clipline Cloud:older-b".into(),
        "Clipline Cloud:older-a".into(),
    ];
    document.cloud.upload_generation_sequence = 47;
    document.cloud.uploads.insert(
        "clip-17".into(),
        CloudUploadRecord {
            local_clip_id: "clip-17".into(),
            client_clip_id: Some("client-17".into()),
            upload_generation: Some(47),
            path: r"C:\Clips\clip-17.mp4".into(),
            remote_clip_id: Some("remote-17".into()),
            remote_url: Some("https://clips.example/c/remote-17".into()),
            visibility: "unlisted".into(),
            upload_status: "uploaded_ready".into(),
            error: None,
            updated_at_unix: 1_700_000_000,
        },
    );
    document.osu = OsuApiSettings {
        client_id: Some("12345".into()),
        user: Some("player".into()),
        credential_target: Some("Clipline osu!:12345:player".into()),
        credential_cleanup_targets: vec!["Clipline osu!:old".into()],
        last_connected_username: Some("Player Name".into()),
    };
    document
}

fn backend_owned_cloud_bytes(document: &AppSettings) -> Vec<u8> {
    let mut cloud = serde_json::to_value(&document.cloud).unwrap();
    let object = cloud.as_object_mut().unwrap();
    object.remove("default_visibility");
    object.remove("delete_local_after_upload");
    object.remove("auto_upload_rules");
    serde_json::to_vec(&cloud).unwrap()
}

#[test]
fn preferences_round_trip_every_ui_owned_field() {
    let mut document = backend_populated_document();
    document.capture_mode = CaptureMode::DisplayRegion;
    document.capture_backend = CaptureBackend::DesktopDuplication;
    document.window_title = "Legacy window title".into();
    document.capture_region = CaptureRegionSettings {
        display_id: Some("display-2".into()),
        x: -1_920,
        y: -200,
        width: 1_280,
        height: 720,
    };
    document.games = GameSettings {
        auto_detect: false,
        pause_when_no_game: true,
        plugins: BTreeMap::from([(
            "league_of_legends".into(),
            GamePluginSettings {
                enabled: true,
                recording_mode: GameRecordingMode::FullSession,
                review: Default::default(),
            },
        )]),
        custom_games: vec![CustomGameSettings {
            id: "custom-notepad".into(),
            legacy_ids: vec!["notepad".into()],
            name: "Notepad".into(),
            enabled: true,
            exe_name: "notepad.exe".into(),
            process_path: Some(r"C:\Windows\System32\notepad.exe".into()),
            window_title: "Untitled - Notepad".into(),
            recording_mode: GameRecordingMode::ReplaysOnly,
            icon: None,
        }],
    };
    document.audio.output_enabled = false;
    document.audio.output_device_id = Some("speakers".into());
    document.audio.output_volume = 0.75;
    document.audio.split_output_by_process = true;
    document.audio.mic_enabled = true;
    document.audio.mic_device_id = Some("microphone".into());
    document.audio.mic_volume = 1.25;
    document.audio.mic_channels = AudioChannelMode::Stereo;
    document.replay_window_s = 90.0;
    document.buffer_seconds = 90.0;
    document.video_quality = VideoQuality::Sharp;
    // The persisted compatibility bitrate mirrors the active advanced mode.
    document.bitrate_mbps = 32.0;
    document.fps = 120;
    document.advanced_recording = AdvancedRecordingSettings {
        enabled: true,
        output_width: 2_560,
        output_height: 1_440,
        bitrate_mbps: 32.0,
        fps: 144,
    };
    document.video_encoder = VideoEncoder::NvencH264;
    document.output_resolution = OutputResolution::P1440;
    document.disk_quota_gb = 25.0;
    document.hotkey = "Ctrl+Shift+F9".into();
    document.hotkey_secondary = Some("Alt+F10".into());
    document.open_on_startup = true;
    document.close_to_tray = false;
    document.minimize_to_tray = true;
    document.legacy_timeline_editor = true;
    document.ui_theme = UiTheme::Classic;
    document.update_channel = UpdateChannel::Nightly;
    document.cloud.default_visibility = "unlisted".into();
    document.cloud.delete_local_after_upload = true;
    document.cloud.auto_upload_rules = true;

    let preferences = SettingsPreferences::from_document(&document).unwrap();
    let mut target = backend_populated_document();
    preferences.apply_to_document(&mut target).unwrap();

    assert_eq!(
        SettingsPreferences::from_document(&target).unwrap(),
        preferences
    );
    assert_eq!(target.buffer_seconds, target.replay_window_s);
}

#[test]
fn applying_preferences_preserves_all_backend_owned_cloud_and_osu_bytes() {
    let mut document = backend_populated_document();
    let cloud_before = document.cloud.clone();
    let cloud_bytes_before = serde_json::to_vec(&document.cloud).unwrap();
    let backend_cloud_bytes_before = backend_owned_cloud_bytes(&document);
    let osu_bytes_before = serde_json::to_vec(&document.osu).unwrap();
    let mut preferences = SettingsPreferences::from_document(&document).unwrap();
    preferences.cloud = CloudUploadPreferences {
        default_visibility: "public".into(),
        delete_local_after_upload: true,
        auto_upload_rules: true,
    };
    preferences.replay_window_s = 75.0;

    preferences.apply_to_document(&mut document).unwrap();

    assert_eq!(document.cloud.host_url, cloud_before.host_url);
    assert_eq!(document.cloud.public_url, cloud_before.public_url);
    assert_eq!(
        document.cloud.connected_user_id,
        cloud_before.connected_user_id
    );
    assert_eq!(
        document.cloud.connected_username,
        cloud_before.connected_username
    );
    assert_eq!(
        document.cloud.connected_display_name,
        cloud_before.connected_display_name
    );
    assert_eq!(
        document.cloud.credential_target,
        cloud_before.credential_target
    );
    assert_eq!(
        document.cloud.credential_cleanup_targets,
        cloud_before.credential_cleanup_targets
    );
    assert_eq!(
        document.cloud.upload_generation_sequence,
        cloud_before.upload_generation_sequence
    );
    assert_eq!(document.cloud.uploads, cloud_before.uploads);
    assert_eq!(
        backend_owned_cloud_bytes(&document),
        backend_cloud_bytes_before
    );
    assert_eq!(serde_json::to_vec(&document.osu).unwrap(), osu_bytes_before);

    let mut expected_cloud = cloud_before;
    expected_cloud.default_visibility = "public".into();
    expected_cloud.delete_local_after_upload = true;
    expected_cloud.auto_upload_rules = true;
    assert_eq!(
        serde_json::to_vec(&document.cloud).unwrap(),
        serde_json::to_vec(&expected_cloud).unwrap()
    );
    assert_ne!(
        serde_json::to_vec(&document.cloud).unwrap(),
        cloud_bytes_before
    );
}

#[test]
fn apply_normalizes_ui_values_and_derives_compatibility_fields() {
    let mut document = backend_populated_document();
    document.buffer_seconds = 5.0;
    let mut preferences = SettingsPreferences::from_document(&document).unwrap();
    preferences.replay_window_s = 80.0;
    preferences.hotkey = "ctrl+shift+f9".into();
    preferences.hotkey_secondary = Some("   ".into());
    preferences.cloud.default_visibility = "PUBLIC".into();
    preferences.games.plugins = vec![GamePluginPreference {
        id: " League of Legends ".into(),
        settings: GamePluginSettings::default(),
    }];
    preferences.advanced_recording = AdvancedRecordingSettings {
        enabled: true,
        output_width: 641,
        output_height: 361,
        bitrate_mbps: 18.0,
        fps: 143,
    };

    preferences.apply_to_document(&mut document).unwrap();

    assert_eq!(document.hotkey, "Ctrl+Shift+F9");
    assert_eq!(document.hotkey_secondary, None);
    assert_eq!(document.cloud.default_visibility, "public");
    assert!(document.games.plugins.contains_key("league_of_legends"));
    assert_eq!(document.advanced_recording.output_width, 642);
    assert_eq!(document.advanced_recording.output_height, 362);
    assert_eq!(document.buffer_seconds, 80.0);
}

#[test]
fn invalid_preferences_fail_without_partially_mutating_the_document() {
    let mut document = backend_populated_document();
    let before = document.clone();
    let mut preferences = SettingsPreferences::from_document(&document).unwrap();
    preferences.hotkey = "Ctrl+F9".into();
    preferences.hotkey_secondary = Some("ctrl+f9".into());
    preferences.cloud.delete_local_after_upload = true;

    let error = preferences.apply_to_document(&mut document).unwrap_err();

    assert!(error.contains("secondary hotkey matches"), "{error}");
    assert_eq!(document, before);
}

#[test]
fn preference_validation_bounds_plugin_and_custom_game_collections() {
    let document = backend_populated_document();
    let mut preferences = SettingsPreferences::from_document(&document).unwrap();
    preferences.games.plugins = (0..=MAX_SETTINGS_GAME_PLUGINS)
        .map(|index| GamePluginPreference {
            id: format!("plugin-{index}"),
            settings: GamePluginSettings::default(),
        })
        .collect();
    assert!(preferences
        .normalized()
        .unwrap_err()
        .contains("game plugins"));

    let mut preferences = SettingsPreferences::from_document(&document).unwrap();
    preferences.games.custom_games = (0..=MAX_SETTINGS_CUSTOM_GAMES)
        .map(|index| CustomGameSettings {
            id: format!("custom-game-{index}"),
            legacy_ids: Vec::new(),
            name: format!("Game {index}"),
            enabled: true,
            exe_name: format!("game-{index}.exe"),
            process_path: None,
            window_title: String::new(),
            recording_mode: GameRecordingMode::ReplaysOnly,
            icon: None,
        })
        .collect();
    assert!(preferences
        .normalized()
        .unwrap_err()
        .contains("custom games"));
}

#[test]
fn preferences_projection_has_no_buffer_or_backend_owned_secret_fields() {
    let document = backend_populated_document();
    let json =
        serde_json::to_value(SettingsPreferences::from_document(&document).unwrap()).unwrap();
    let object = json.as_object().unwrap();

    assert!(!object.contains_key("buffer_seconds"));
    assert!(!object.contains_key("osu"));
    let cloud = object["cloud"].as_object().unwrap();
    assert_eq!(cloud.len(), 3);
    assert!(!cloud.contains_key("host_url"));
    assert!(!cloud.contains_key("credential_target"));
    assert!(!cloud.contains_key("uploads"));
}

#[test]
fn replay_storage_mode_remains_an_ui_owned_preference() {
    let mut document = backend_populated_document();
    document.replay_storage.mode = ReplayStorageMode::Memory;
    let mut preferences = SettingsPreferences::from_document(&document).unwrap();
    preferences.replay_storage.mode = ReplayStorageMode::Memory;
    preferences.apply_to_document(&mut document).unwrap();
    assert_eq!(document.replay_storage.mode, ReplayStorageMode::Memory);
}

#[test]
fn projection_rejects_unbounded_fields_before_publication() {
    let document = AppSettings {
        window_title: "w".repeat(MAX_SETTINGS_FIELD_BYTES + 1),
        ..AppSettings::default()
    };
    assert!(SettingsPreferences::from_document(&document)
        .unwrap_err()
        .contains("window title"));

    let mut document = AppSettings::default();
    document.capture_region.display_id = Some("d".repeat(MAX_SETTINGS_FIELD_BYTES + 1));
    assert!(SettingsPreferences::from_document(&document)
        .unwrap_err()
        .contains("display id"));

    let mut document = AppSettings::default();
    document.audio.output_device_id = Some("a".repeat(MAX_SETTINGS_FIELD_BYTES + 1));
    assert!(SettingsPreferences::from_document(&document)
        .unwrap_err()
        .contains("output device"));

    let mut document = AppSettings::default();
    document.games.plugins.insert(
        "p".repeat(MAX_SETTINGS_FIELD_BYTES + 1),
        GamePluginSettings::default(),
    );
    assert!(SettingsPreferences::from_document(&document)
        .unwrap_err()
        .contains("plugin id"));
}

#[test]
fn projection_rejects_aggregate_custom_game_bytes_before_clone() {
    let mut document = AppSettings::default();
    let icon = format!("data:image/png;base64,{}", "a".repeat(256 * 1024 - 22));
    let count = MAX_SETTINGS_COLLECTION_BYTES / icon.len() + 1;
    document.games.custom_games = (0..count)
        .map(|index| CustomGameSettings {
            id: format!("custom-game-{index}"),
            legacy_ids: Vec::new(),
            name: format!("Game {index}"),
            enabled: true,
            exe_name: format!("game-{index}.exe"),
            process_path: None,
            window_title: String::new(),
            recording_mode: GameRecordingMode::ReplaysOnly,
            icon: Some(icon.clone()),
        })
        .collect();
    assert!(document.games.custom_games.len() <= MAX_SETTINGS_CUSTOM_GAMES);

    let error = SettingsPreferences::from_document(&document).unwrap_err();
    assert!(error.contains("aggregate"), "{error}");
}

#[test]
fn hidden_capture_and_replay_values_are_validated() {
    let document = AppSettings {
        capture_mode: CaptureMode::PrimaryMonitor,
        capture_region: CaptureRegionSettings {
            width: 1,
            ..CaptureRegionSettings::default()
        },
        ..AppSettings::default()
    };
    assert!(SettingsPreferences::from_document(&document)
        .unwrap_err()
        .contains("capture region"));

    let mut document = AppSettings::default();
    document.replay_storage.mode = ReplayStorageMode::Memory;
    document.replay_storage.disk_quota_gb = f64::NAN;
    assert!(SettingsPreferences::from_document(&document)
        .unwrap_err()
        .contains("replay cache quota"));
}
