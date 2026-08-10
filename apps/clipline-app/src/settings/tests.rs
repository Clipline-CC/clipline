use super::*;
use crate::service::{
    AudioChannelMode, AudioOptions, CaptureRegion, CaptureSource, ReplayStorageOptions,
    VideoEncoder, DEFAULT_DISK_QUOTA_BYTES,
};
use crate::settings::persistence::sibling_tmp_path;
use crate::settings::types::ReplayStorageMode;
use std::collections::BTreeMap;
use std::path::PathBuf;

use clipline_test_utils::TestDir;
use serde_json::Value;

#[test]
fn defaults_match_current_recorder_behavior() {
    let settings = AppSettings::default();

    assert_eq!(settings.capture_mode, CaptureMode::PrimaryMonitor);
    assert!(settings.games.auto_detect);
    assert!(!settings.games.pause_when_no_game);
    assert!(settings.games.plugins.is_empty());
    assert!(settings.games.custom_games.is_empty());
    assert!(settings.audio.output_enabled);
    assert_eq!(settings.audio.output_device_id, None);
    assert_eq!(settings.audio.output_volume, 1.0);
    assert!(!settings.audio.split_output_by_process);
    let serialized = serde_json::to_value(&settings).unwrap();
    assert_eq!(serialized["games"]["pause_when_no_game"], false);
    assert_eq!(serialized["audio"]["split_output_by_process"], false);
    assert!(!settings.audio.mic_enabled);
    assert_eq!(settings.audio.mic_device_id, None);
    assert_eq!(settings.audio.mic_volume, 1.0);
    assert_eq!(settings.audio.mic_channels, AudioChannelMode::Mono);
    assert_eq!(settings.replay_window_s, 60.0);
    assert_eq!(settings.buffer_seconds, 60.0);
    assert_eq!(settings.video_quality, VideoQuality::Balanced);
    assert_eq!(settings.bitrate_mbps, 12.0);
    assert_eq!(settings.fps, 60);
    assert_eq!(settings.video_encoder, VideoEncoder::Auto);
    assert_eq!(settings.output_resolution, OutputResolution::Source);
    assert_eq!(settings.disk_quota_gb, 10.0);
    assert!(!settings.auto_delete_when_over_quota);
    assert_eq!(settings.media_dir, default_media_dir());
    assert_eq!(settings.replay_storage, ReplayStorageSettings::default());
    assert_eq!(settings.hotkey, "F6");
    assert!(!settings.open_on_startup);
    assert!(settings.close_to_tray);
    assert!(!settings.minimize_to_tray);
    assert_eq!(settings.update_channel, UpdateChannel::Nightly);
    assert!(!settings.legacy_timeline_editor);
    assert_eq!(serialized["legacy_timeline_editor"], false);
}

#[test]
fn ui_themes_round_trip_through_settings_json() {
    for name in ["booth", "classic", "purple", "oled", "dark", "light"] {
        let theme: UiTheme = serde_json::from_value(name.into()).unwrap();
        assert_eq!(serde_json::to_value(theme).unwrap(), name);
    }
}

#[test]
fn legacy_games_default_no_game_pause_off() {
    let settings: GameSettings = serde_json::from_str(
        r#"{
            "auto_detect": true,
            "custom_games": []
        }"#,
    )
    .unwrap();

    assert!(settings.auto_detect);
    assert!(!settings.pause_when_no_game);
    let saved = serde_json::to_value(&settings).unwrap();
    assert_eq!(saved["pause_when_no_game"], false);
}

#[test]
fn validation_rejects_replay_longer_than_two_minutes() {
    let settings = AppSettings {
        replay_window_s: 121.0,
        buffer_seconds: 300.0,
        ..AppSettings::default()
    };

    assert!(settings.validate().is_err());
}

#[test]
fn validation_requires_window_title_for_window_capture() {
    let settings = AppSettings {
        capture_mode: CaptureMode::WindowTitle,
        window_title: " ".into(),
        ..AppSettings::default()
    };

    assert!(settings.validate().is_err());
}

#[test]
fn legacy_settings_default_capture_region() {
    let json = r#"{
            "capture_mode": "primary_monitor",
            "window_title": "",
            "buffer_seconds": 120.0,
            "replay_window_s": 60.0,
            "bitrate_mbps": 12.0,
            "fps": 60,
            "disk_quota_gb": 10.0,
            "hotkey": "Alt+F10"
        }"#;
    let settings = AppSettings::load_from_object(
        serde_json::from_str::<Value>(json)
            .unwrap()
            .as_object()
            .unwrap(),
    );

    assert_eq!(settings.capture_region.width, 1920);
    assert_eq!(settings.capture_region.height, 1080);
    assert_eq!(settings.audio, AudioSettings::default());
    assert_eq!(settings.games, GameSettings::default());
    assert_eq!(settings.media_dir, default_media_dir());
    assert_eq!(settings.video_encoder, VideoEncoder::Auto);
    assert_eq!(settings.output_resolution, OutputResolution::Source);
    assert!(settings.close_to_tray);
    assert!(!settings.minimize_to_tray);
    assert_eq!(settings.update_channel, UpdateChannel::Nightly);
    assert!(settings.validate().is_ok());
}

#[test]
fn load_preserves_legacy_timeline_editor_preference() {
    let settings = AppSettings::load_from_object(
        serde_json::from_str::<Value>(
            r#"{
                "capture_mode": "primary_monitor",
                "window_title": "",
                "buffer_seconds": 75.0,
                "replay_window_s": 60.0,
                "bitrate_mbps": 12.0,
                "fps": 60,
                "disk_quota_gb": 10.0,
                "hotkey": "Alt+F10",
                "legacy_timeline_editor": true
            }"#,
        )
        .unwrap()
        .as_object()
        .unwrap(),
    );

    assert!(settings.legacy_timeline_editor);
}

#[test]
fn load_repairs_disabled_stable_update_channel() {
    let dir = TestDir::new("clipline-settings", "stable-update-channel");
    let path = dir.path().join("settings.json");
    std::fs::write(
        &path,
        r#"{
                "capture_mode": "primary_monitor",
                "window_title": "",
                "replay_window_s": 60.0,
                "bitrate_mbps": 12.0,
                "fps": 60,
                "disk_quota_gb": 10.0,
                "hotkey": "Alt+F10",
                "update_channel": "stable"
            }"#,
    )
    .unwrap();

    let settings = AppSettings::load_from(&path).unwrap();

    assert_eq!(settings.update_channel, UpdateChannel::Nightly);
}

#[test]
fn legacy_custom_games_default_to_replays_only() {
    let json = r#"{
            "capture_mode": "primary_monitor",
            "window_title": "",
            "buffer_seconds": 120.0,
            "replay_window_s": 60.0,
            "bitrate_mbps": 12.0,
            "fps": 60,
            "disk_quota_gb": 10.0,
            "hotkey": "Alt+F10",
            "games": {
                "auto_detect": true,
                "custom_games": [{
                    "id": "custom-test",
                    "name": "Test Game",
                    "enabled": true,
                    "exe_name": "game.exe",
                    "process_path": "C:\\Games\\Test\\game.exe",
                    "window_title": "Test Game"
                }]
            }
        }"#;
    let settings = AppSettings::load_from_object(
        serde_json::from_str::<Value>(json)
            .unwrap()
            .as_object()
            .unwrap(),
    );

    assert_eq!(
        settings.games.custom_games[0].recording_mode,
        GameRecordingMode::ReplaysOnly
    );
    assert!(settings.validate().is_ok());
}

#[test]
fn custom_game_normalization_deduplicates_match_identity_and_keeps_last() {
    let mut games = GameSettings {
        custom_games: vec![
            CustomGameSettings {
                id: "custom-ffxiv-old".into(),
                legacy_ids: Vec::new(),
                name: "ffxiv_dx11".into(),
                enabled: true,
                exe_name: "ffxiv_dx11.exe".into(),
                process_path: Some(r"D:\Games\FFXIV\ffxiv_dx11.exe".into()),
                window_title: "FINAL FANTASY XIV".into(),
                recording_mode: GameRecordingMode::ReplaysOnly,
                icon: None,
            },
            CustomGameSettings {
                id: "custom-ffxiv-new".into(),
                legacy_ids: Vec::new(),
                name: "ffxiv_dx11".into(),
                enabled: false,
                exe_name: "FFXIV_DX11.EXE".into(),
                process_path: Some("d:/games/ffxiv/ffxiv_dx11.exe/".into()),
                window_title: "FINAL FANTASY XIV".into(),
                recording_mode: GameRecordingMode::ReplaysOnly,
                icon: None,
            },
        ],
        ..GameSettings::default()
    };

    games.normalize();

    assert_eq!(games.custom_games.len(), 1);
    assert_eq!(games.custom_games[0].id, "custom-ffxiv-new");
    assert!(!games.custom_games[0].enabled);
    assert_eq!(games.custom_games[0].legacy_ids, ["custom-ffxiv-old"]);
}

#[test]
fn custom_game_normalization_preserves_distinct_window_rules_for_one_executable() {
    let base = CustomGameSettings {
        id: "custom-launcher".into(),
        legacy_ids: Vec::new(),
        name: "Game launcher".into(),
        enabled: true,
        exe_name: "game.exe".into(),
        process_path: Some(r"C:\Games\Game\game.exe".into()),
        window_title: "Launcher".into(),
        recording_mode: GameRecordingMode::ReplaysOnly,
        icon: None,
    };
    let mut games = GameSettings {
        custom_games: vec![
            base.clone(),
            CustomGameSettings {
                id: "custom-match".into(),
                name: "Game match".into(),
                window_title: "In game".into(),
                ..base
            },
        ],
        ..GameSettings::default()
    };

    games.normalize();

    assert_eq!(games.custom_games.len(), 2);
    assert_eq!(games.custom_games[0].window_title, "Launcher");
    assert_eq!(games.custom_games[1].window_title, "In game");
}

#[test]
fn legacy_global_game_recording_mode_migrates_to_custom_games() {
    let json = r#"{
            "capture_mode": "primary_monitor",
            "window_title": "",
            "buffer_seconds": 120.0,
            "replay_window_s": 60.0,
            "bitrate_mbps": 12.0,
            "fps": 60,
            "disk_quota_gb": 10.0,
            "hotkey": "Alt+F10",
            "games": {
                "auto_detect": true,
                "recording_mode": "full_session",
                "custom_games": [{
                    "id": "custom-test",
                    "name": "Test Game",
                    "enabled": true,
                    "exe_name": "game.exe",
                    "process_path": "C:\\Games\\Test\\game.exe",
                    "window_title": "Test Game"
                }]
            }
        }"#;
    let settings: AppSettings = serde_json::from_str(json).unwrap();

    assert_eq!(
        settings.games.custom_games[0].recording_mode,
        GameRecordingMode::FullSession
    );
    let saved = serde_json::to_value(&settings).unwrap();
    assert!(saved["games"].get("recording_mode").is_none());
}

#[test]
fn legacy_custom_game_ids_migrate_out_of_the_built_in_namespace() {
    let settings = AppSettings::load_from_object(
        serde_json::json!({
            "games": {
                "custom_games": [
                    {
                        "id": "osu",
                        "name": "My Rhythm Tool",
                        "exe_name": "rhythm.exe",
                        "icon": "data:image/png;base64,aWNvbg=="
                    },
                    {
                        "id": "league_of_legends",
                        "name": "Spreadsheet League",
                        "exe_name": "sheets.exe"
                    },
                    {
                        "id": "My Old Game!",
                        "name": "Old Game",
                        "exe_name": "old.exe"
                    }
                ]
            }
        })
        .as_object()
        .unwrap(),
    );

    let games = &settings.games.custom_games;
    assert_eq!(games[0].id, "custom-migrated-osu");
    assert_eq!(games[1].id, "custom-migrated-league-of-legends");
    assert_eq!(games[2].id, "custom-migrated-my-old-game");
    assert_eq!(games[0].legacy_ids, ["osu"]);
    assert_eq!(games[1].legacy_ids, ["league_of_legends"]);
    assert_eq!(games[2].legacy_ids, ["My Old Game!"]);
    assert_eq!(games[0].name, "My Rhythm Tool");
    assert_eq!(
        games[0].icon.as_deref(),
        Some("data:image/png;base64,aWNvbg==")
    );
    assert!(settings.validate().is_ok());
}

#[test]
fn legacy_custom_game_id_migration_is_unique_and_idempotent() {
    let mut games = GameSettings {
        custom_games: vec![
            CustomGameSettings {
                id: "osu".into(),
                legacy_ids: Vec::new(),
                name: "First".into(),
                enabled: true,
                exe_name: "first.exe".into(),
                process_path: None,
                window_title: String::new(),
                recording_mode: GameRecordingMode::ReplaysOnly,
                icon: None,
            },
            CustomGameSettings {
                id: "OSU".into(),
                legacy_ids: Vec::new(),
                name: "Second".into(),
                enabled: true,
                exe_name: "second.exe".into(),
                process_path: None,
                window_title: String::new(),
                recording_mode: GameRecordingMode::ReplaysOnly,
                icon: None,
            },
            CustomGameSettings {
                id: "custom-migrated-osu".into(),
                legacy_ids: Vec::new(),
                name: "Existing".into(),
                enabled: true,
                exe_name: "existing.exe".into(),
                process_path: None,
                window_title: String::new(),
                recording_mode: GameRecordingMode::ReplaysOnly,
                icon: None,
            },
        ],
        ..GameSettings::default()
    };

    games.normalize();
    let first = games
        .custom_games
        .iter()
        .map(|game| game.id.clone())
        .collect::<Vec<_>>();
    games.normalize();
    let second = games
        .custom_games
        .iter()
        .map(|game| game.id.clone())
        .collect::<Vec<_>>();

    assert_eq!(
        first,
        vec![
            "custom-migrated-osu-2",
            "custom-migrated-osu-3",
            "custom-migrated-osu"
        ]
    );
    assert_eq!(second, first);
}

#[test]
fn validation_rejects_custom_ids_outside_the_custom_namespace() {
    for id in ["osu", "league_of_legends", "plain-legacy-id", "custom-Bad"] {
        let settings = AppSettings {
            games: GameSettings {
                custom_games: vec![CustomGameSettings {
                    id: id.into(),
                    legacy_ids: Vec::new(),
                    name: "Impostor".into(),
                    enabled: true,
                    exe_name: "impostor.exe".into(),
                    process_path: None,
                    window_title: String::new(),
                    recording_mode: GameRecordingMode::ReplaysOnly,
                    icon: None,
                }],
                ..GameSettings::default()
            },
            ..AppSettings::default()
        };

        assert!(settings.validate().is_err(), "{id:?} must be rejected");
    }
}

#[test]
fn supported_game_review_settings_round_trip_json() {
    let json = r#"{
            "games": {
                "auto_detect": true,
                "plugins": {
                    "league_of_legends": {
                        "enabled": true,
                        "recording_mode": "full_session",
                        "review": {
                            "enabled": false,
                            "match_events": {
                                "enabled": true,
                                "user_kills": true,
                                "user_deaths": false,
                                "user_assists": true,
                                "team_kills": false,
                                "team_deaths": true,
                                "enemy_kills": false,
                                "enemy_deaths": true,
                                "objectives": false,
                                "turrets": true
                            },
                            "timeline_markers": {
                                "enabled": true,
                                "user_kills": false,
                                "user_deaths": true,
                                "user_assists": false,
                                "objectives": true,
                                "turrets": false
                            }
                        }
                    }
                }
            }
        }"#;
    let settings = AppSettings::load_from_object(
        serde_json::from_str::<Value>(json)
            .unwrap()
            .as_object()
            .unwrap(),
    );
    let saved = serde_json::to_value(&settings).unwrap();
    let review = &saved["games"]["plugins"]["league_of_legends"]["review"];

    assert_eq!(review["enabled"], false);
    assert_eq!(review["match_events"]["user_deaths"], false);
    assert_eq!(review["match_events"]["team_kills"], false);
    assert_eq!(review["match_events"]["enemy_deaths"], true);
    assert_eq!(review["match_events"]["objectives"], false);
    assert_eq!(review["timeline_markers"]["user_kills"], false);
    assert_eq!(review["timeline_markers"]["user_assists"], false);
    assert_eq!(review["timeline_markers"]["turrets"], false);
}

#[test]
fn supported_game_review_settings_default_to_current_enhanced_view() {
    let settings = AppSettings {
        games: GameSettings {
            auto_detect: true,
            pause_when_no_game: false,
            plugins: BTreeMap::from([(
                "league_of_legends".into(),
                GamePluginSettings {
                    enabled: true,
                    recording_mode: GameRecordingMode::FullSession,
                    review: Default::default(),
                },
            )]),
            custom_games: Vec::new(),
        },
        ..AppSettings::default()
    };

    let saved = serde_json::to_value(&settings).unwrap();
    let review = &saved["games"]["plugins"]["league_of_legends"]["review"];
    assert_eq!(review["enabled"], true);
    assert_eq!(review["match_events"]["enabled"], true);
    assert_eq!(review["match_events"]["user_kills"], true);
    assert_eq!(review["match_events"]["user_deaths"], true);
    assert_eq!(review["match_events"]["user_assists"], true);
    assert_eq!(review["match_events"]["team_kills"], true);
    assert_eq!(review["match_events"]["team_deaths"], true);
    assert_eq!(review["match_events"]["enemy_kills"], true);
    assert_eq!(review["match_events"]["enemy_deaths"], true);
    assert_eq!(review["match_events"]["objectives"], true);
    assert_eq!(review["match_events"]["turrets"], true);
    assert_eq!(review["timeline_markers"]["enabled"], true);
    assert_eq!(review["timeline_markers"]["user_kills"], true);
    assert_eq!(review["timeline_markers"]["user_deaths"], true);
    assert_eq!(review["timeline_markers"]["user_assists"], true);
    assert_eq!(review["timeline_markers"]["objectives"], true);
    assert_eq!(review["timeline_markers"]["turrets"], true);
}

#[test]
fn validation_rejects_relative_media_folder() {
    let settings = AppSettings {
        media_dir: "clips".into(),
        ..AppSettings::default()
    };

    assert!(settings.validate().is_err());
}

#[test]
fn validation_rejects_filesystem_and_sensitive_media_roots() {
    let current = std::env::current_dir().unwrap();
    let filesystem_root = current.ancestors().last().unwrap();
    let root_settings = AppSettings {
        media_dir: filesystem_root.display().to_string(),
        ..AppSettings::default()
    };
    assert!(root_settings.validate().is_err());

    for variable in [
        "USERPROFILE",
        "SystemRoot",
        "ProgramData",
        "ProgramFiles",
        "ProgramFiles(x86)",
    ] {
        let Some(root) = std::env::var_os(variable).map(PathBuf::from) else {
            continue;
        };
        if !root.is_absolute() {
            continue;
        }
        let settings = AppSettings {
            media_dir: root.display().to_string(),
            ..AppSettings::default()
        };
        assert!(
            settings.validate().is_err(),
            "{variable} root must be rejected"
        );
    }

    let nested = std::env::temp_dir().join("clipline-media-scope-test");
    let settings = AppSettings {
        media_dir: nested.display().to_string(),
        ..AppSettings::default()
    };
    assert!(settings.validate().is_ok());
}

#[test]
fn load_heals_invalid_media_folder_without_resetting_settings() {
    let dir = TestDir::new("clipline-settings", "heal-media-folder");
    let path = dir.path().join("settings.json");
    std::fs::write(
        &path,
        r#"{
                "capture_mode": "primary_monitor",
                "window_title": "",
                "buffer_seconds": 120.0,
                "replay_window_s": 60.0,
                "bitrate_mbps": 24.0,
                "fps": 90,
                "media_dir": "relative/not/allowed",
                "disk_quota_gb": 6.0,
                "hotkey": "Alt+F9"
            }"#,
    )
    .unwrap();

    let settings = AppSettings::load_from(&path).unwrap();

    assert_eq!(settings.media_dir, default_media_dir());
    assert_eq!(settings.bitrate_mbps, 24.0);
    assert_eq!(settings.fps, 90);
    assert_eq!(settings.disk_quota_gb, 6.0);
    assert_eq!(settings.hotkey, "Alt+F9");
}

#[test]
fn legacy_bitrate_migration_uses_output_resolution() {
    let dir = TestDir::new("clipline-settings", "legacy-quality-resolution");
    let path = dir.path().join("settings.json");
    std::fs::write(
        &path,
        r#"{
                "capture_mode": "primary_monitor",
                "window_title": "",
                "buffer_seconds": 120.0,
                "replay_window_s": 60.0,
                "output_resolution": "720p",
                "bitrate_mbps": 5.0,
                "fps": 60,
                "disk_quota_gb": 10.0,
                "hotkey": "Alt+F10"
            }"#,
    )
    .unwrap();

    let settings = AppSettings::load_from(&path).unwrap();

    assert_eq!(settings.output_resolution, OutputResolution::P720);
    assert_eq!(settings.video_quality, VideoQuality::Balanced);
    assert_eq!(settings.bitrate_mbps, 5.0);
    assert_eq!(
        settings.to_service_options(None).unwrap().bitrate_bps,
        5_000_000
    );
}

#[test]
fn validation_rejects_out_of_range_audio_volume() {
    let settings = AppSettings {
        audio: AudioSettings {
            output_volume: 2.1,
            ..AudioSettings::default()
        },
        ..AppSettings::default()
    };

    assert!(settings.validate().is_err());

    let settings = AppSettings {
        audio: AudioSettings {
            mic_volume: -0.1,
            ..AudioSettings::default()
        },
        ..AppSettings::default()
    };

    assert!(settings.validate().is_err());
}

#[test]
fn load_clamps_legacy_replay_window_to_two_minutes() {
    let dir = TestDir::new("clipline-settings", "clamp-replay-window");
    let path = dir.path().join("settings.json");
    std::fs::write(
        &path,
        r#"{
                "capture_mode": "primary_monitor",
                "window_title": "",
                "buffer_seconds": 300.0,
                "replay_window_s": 300.0,
                "bitrate_mbps": 12.0,
                "fps": 60,
                "disk_quota_gb": 10.0,
                "hotkey": "Alt+F10"
            }"#,
    )
    .unwrap();

    let settings = AppSettings::load_from(&path).unwrap();

    assert_eq!(settings.replay_window_s, 120.0);
    // The legacy compatibility field mirrors the (clamped) save window.
    assert_eq!(settings.buffer_seconds, 120.0);
}

#[test]
fn load_migrates_invalid_legacy_hotkey_without_resetting_settings() {
    let dir = TestDir::new("clipline-settings", "migrate-hotkey");
    let path = dir.path().join("settings.json");
    std::fs::write(
        &path,
        r#"{
                "capture_mode": "display_region",
                "window_title": "",
                "capture_region": {
                    "display_id": "\\\\.\\DISPLAY2",
                    "x": 1920,
                    "y": 0,
                    "width": 1280,
                    "height": 720
                },
                "buffer_seconds": 120.0,
                "replay_window_s": 60.0,
                "bitrate_mbps": 24.0,
                "fps": 90,
                "disk_quota_gb": 6.0,
                "hotkey": "F12"
            }"#,
    )
    .unwrap();

    let settings = AppSettings::load_from(&path).unwrap();

    assert_eq!(settings.hotkey, "F6");
    assert_eq!(settings.capture_mode, CaptureMode::DisplayRegion);
    assert_eq!(settings.capture_region.width, 1280);
    assert_eq!(settings.bitrate_mbps, 24.0);
    assert_eq!(settings.fps, 90);
    assert_eq!(settings.disk_quota_gb, 6.0);
}

#[test]
fn load_drops_invalid_or_duplicate_secondary_hotkey() {
    let dir = TestDir::new("clipline-settings", "secondary-hotkey-repair");
    let path = dir.path().join("settings.json");
    std::fs::write(
        &path,
        r#"{
                "capture_mode": "primary_monitor",
                "window_title": "",
                "buffer_seconds": 120.0,
                "replay_window_s": 60.0,
                "bitrate_mbps": 24.0,
                "fps": 90,
                "disk_quota_gb": 6.0,
                "hotkey": "Alt+F9",
                "hotkey_secondary": "F12"
            }"#,
    )
    .unwrap();
    let settings = AppSettings::load_from(&path).unwrap();
    assert_eq!(settings.hotkey, "Alt+F9");
    assert_eq!(settings.hotkey_secondary, None);

    std::fs::write(
        &path,
        r#"{
                "capture_mode": "primary_monitor",
                "window_title": "",
                "buffer_seconds": 120.0,
                "replay_window_s": 60.0,
                "bitrate_mbps": 24.0,
                "fps": 90,
                "disk_quota_gb": 6.0,
                "hotkey": "Alt+F9",
                "hotkey_secondary": "alt+f9"
            }"#,
    )
    .unwrap();
    let settings = AppSettings::load_from(&path).unwrap();
    assert_eq!(settings.hotkey, "Alt+F9");
    assert_eq!(settings.hotkey_secondary, None);
}

#[test]
fn secondary_hotkey_round_trips_and_lists_both_keybinds() {
    let dir = TestDir::new("clipline-settings", "secondary-hotkey-round-trip");
    let path = dir.path().join("settings.json");
    let settings = AppSettings {
        hotkey: "Alt+F10".into(),
        hotkey_secondary: Some("Ctrl+Mouse5".into()),
        ..AppSettings::default()
    };

    assert_eq!(settings.hotkeys(), vec!["Alt+F10", "Ctrl+Mouse5"]);

    settings.save_to(&path).unwrap();
    let loaded = AppSettings::load_from(&path).unwrap();
    assert_eq!(loaded.hotkey_secondary.as_deref(), Some("Ctrl+Mouse5"));
}

#[test]
fn validation_rejects_secondary_hotkey_matching_primary() {
    let settings = AppSettings {
        hotkey: "Alt+F10".into(),
        hotkey_secondary: Some("alt+f10".into()),
        ..AppSettings::default()
    };
    assert!(settings.validate().is_err());

    let distinct = AppSettings {
        hotkey: "Alt+F10".into(),
        hotkey_secondary: Some("Ctrl+F9".into()),
        ..AppSettings::default()
    };
    assert!(distinct.validate().is_ok());

    let blank = AppSettings {
        hotkey_secondary: Some("  ".into()),
        ..AppSettings::default()
    };
    assert!(blank.validate().is_ok());
    assert_eq!(blank.hotkeys(), vec!["F6"]);
}

#[test]
fn recording_hotkeys_are_optional_normalized_and_distinct_from_each_other_and_save_replay() {
    assert_eq!(AppSettings::default().recording_hotkey, None);
    assert_eq!(AppSettings::default().recording_hotkey_secondary, None);

    let dir = TestDir::new("clipline-settings", "recording-hotkey-round-trip");
    let path = dir.path().join("settings.json");
    let settings = AppSettings {
        recording_hotkey: Some("ctrl+shift+r".into()),
        recording_hotkey_secondary: Some("alt+mouse4".into()),
        ..AppSettings::default()
    };
    settings.save_to(&path).unwrap();
    let loaded = AppSettings::load_from(&path).unwrap();
    assert_eq!(loaded.recording_hotkey.as_deref(), Some("Ctrl+Shift+R"));
    assert_eq!(loaded.recording_hotkey_secondary.as_deref(), Some("Alt+Mouse4"));

    let primary_conflict = AppSettings {
        recording_hotkey: Some("f6".into()),
        ..AppSettings::default()
    };
    assert!(primary_conflict.validate().is_err());

    let secondary_conflict = AppSettings {
        hotkey_secondary: Some("Ctrl+R".into()),
        recording_hotkey: Some("ctrl+r".into()),
        ..AppSettings::default()
    };
    assert!(secondary_conflict.validate().is_err());

    let recording_conflict = AppSettings {
        recording_hotkey: Some("Alt+R".into()),
        recording_hotkey_secondary: Some("alt+r".into()),
        ..AppSettings::default()
    };
    assert!(recording_conflict.validate().is_err());
}

#[test]
fn load_tolerates_unknown_video_encoder_without_resetting_settings() {
    let dir = TestDir::new("clipline-settings", "unknown-encoder");
    let path = dir.path().join("settings.json");
    std::fs::write(
        &path,
        r#"{
                "capture_mode": "primary_monitor",
                "window_title": "",
                "buffer_seconds": 120.0,
                "replay_window_s": 60.0,
                "bitrate_mbps": 24.0,
                "fps": 90,
                "video_encoder": "hevc_av1_turbo",
                "disk_quota_gb": 6.0,
                "hotkey": "Alt+F9"
            }"#,
    )
    .unwrap();

    let settings = AppSettings::load_from(&path).unwrap();

    assert_eq!(settings.video_encoder, VideoEncoder::Auto);
    assert_eq!(settings.bitrate_mbps, 24.0);
    assert_eq!(settings.fps, 90);
    assert_eq!(settings.disk_quota_gb, 6.0);
    assert_eq!(settings.hotkey, "Alt+F9");
}

#[test]
fn load_repairs_invalid_fields_without_resetting_valid_neighbors() {
    let dir = TestDir::new("clipline-settings", "repair-invalid-fields");
    let path = dir.path().join("settings.json");
    std::fs::write(
        &path,
        r#"{
                "capture_mode": "future_capture",
                "capture_backend": "smart_future",
                "window_title": "Keep this window title",
                "capture_region": {
                    "display_id": "\\\\.\\DISPLAY2",
                    "x": 100,
                    "y": 200,
                    "width": 1,
                    "height": 50000
                },
                "audio": {
                    "output_enabled": false,
                    "output_device_id": "speaker-id",
                    "output_volume": 2.5,
                    "mic_enabled": true,
                    "mic_device_id": "   ",
                    "mic_volume": -0.25,
                    "mic_channels": "surround"
                },
                "buffer_seconds": 1.0,
                "replay_window_s": 999.0,
                "bitrate_mbps": 250.0,
                "fps": 999,
                "video_encoder": "av1_future",
                "disk_quota_gb": -1.0,
                "media_dir": "",
                "hotkey": "F12"
            }"#,
    )
    .unwrap();

    let settings = AppSettings::load_from(&path).unwrap();

    assert_eq!(settings.capture_mode, CaptureMode::PrimaryMonitor);
    assert_eq!(settings.capture_backend, CaptureBackend::Auto);
    assert_eq!(settings.window_title, "Keep this window title");
    assert_eq!(
        settings.capture_region.display_id.as_deref(),
        Some(r"\\.\DISPLAY2")
    );
    assert_eq!(settings.capture_region.x, 100);
    assert_eq!(settings.capture_region.y, 200);
    assert_eq!(settings.capture_region.width, 2);
    assert_eq!(settings.capture_region.height, 16_384);
    assert!(!settings.audio.output_enabled);
    assert_eq!(
        settings.audio.output_device_id.as_deref(),
        Some("speaker-id")
    );
    assert_eq!(settings.audio.output_volume, 2.0);
    assert!(settings.audio.mic_enabled);
    assert_eq!(settings.audio.mic_device_id, None);
    assert_eq!(settings.audio.mic_volume, 0.0);
    assert_eq!(settings.audio.mic_channels, AudioChannelMode::Mono);
    assert_eq!(settings.replay_window_s, 120.0);
    assert_eq!(settings.buffer_seconds, 120.0);
    assert_eq!(settings.video_quality, VideoQuality::Maximum);
    assert_eq!(settings.bitrate_mbps, 40.0);
    assert_eq!(settings.fps, 120);
    assert_eq!(settings.video_encoder, VideoEncoder::Auto);
    assert_eq!(settings.disk_quota_gb, 0.0);
    assert_eq!(settings.media_dir, default_media_dir());
    assert_eq!(settings.hotkey, "F6");
    assert!(settings.validate().is_ok());
}

#[test]
fn display_region_settings_round_trip_json() {
    let dir = TestDir::new("clipline-settings", "region-round-trip");
    let path = dir.path().join("settings.json");
    let settings = AppSettings {
        capture_mode: CaptureMode::DisplayRegion,
        capture_region: CaptureRegionSettings {
            display_id: Some(r"\\.\DISPLAY2".into()),
            x: 1920,
            y: 120,
            width: 1280,
            height: 720,
        },
        ..AppSettings::default()
    };

    settings.save_to(&path).unwrap();
    let loaded = AppSettings::load_from(&path).unwrap();

    assert_eq!(loaded, settings);
}

#[test]
fn validation_rejects_too_small_display_region() {
    let settings = AppSettings {
        capture_mode: CaptureMode::DisplayRegion,
        capture_region: CaptureRegionSettings {
            width: 1,
            height: 1080,
            ..CaptureRegionSettings::default()
        },
        ..AppSettings::default()
    };

    assert!(settings.validate().is_err());
}

#[test]
fn service_options_include_estimated_buffer_bytes() {
    let settings = AppSettings::default();
    let opts = settings
        .to_service_options(Some("http://mock".into()))
        .unwrap();

    assert_eq!(opts.replay_window_s, 60.0);
    assert_eq!(opts.fps, 60);
    assert_eq!(opts.bitrate_bps, 12_000_000);
    assert_eq!(opts.video_encoder, VideoEncoder::Auto);
    assert_eq!(opts.output_resolution, OutputResolution::Source);
    assert_eq!(opts.disk_quota_bytes, Some(DEFAULT_DISK_QUOTA_BYTES));
    assert_eq!(opts.media_dir, PathBuf::from(default_media_dir()));
    assert_eq!(opts.lol_url.as_deref(), Some("http://mock"));
    assert_eq!(opts.audio, AudioOptions::default());
    assert_eq!(opts.replay_storage, ReplayStorageOptions::Memory);
    assert_eq!(
        opts.buffer_bytes,
        estimated_buffer_bytes(60.0, settings.effective_bitrate_mbps())
    );
}

#[test]
fn service_options_include_audio_settings() {
    let settings = AppSettings {
        audio: AudioSettings {
            output_enabled: true,
            output_device_id: Some("output-id".into()),
            output_volume: 0.75,
            split_output_by_process: false,
            mic_enabled: true,
            mic_device_id: Some("mic-id".into()),
            mic_volume: 1.5,
            mic_channels: AudioChannelMode::Stereo,
        },
        ..AppSettings::default()
    };

    let opts = settings.to_service_options(None).unwrap();

    assert!(opts.audio.output_enabled);
    assert_eq!(opts.audio.output_device_id.as_deref(), Some("output-id"));
    assert_eq!(opts.audio.output_volume, 0.75);
    assert!(!opts.audio.split_output_by_process);
    assert!(opts.audio.mic_enabled);
    assert_eq!(opts.audio.mic_device_id.as_deref(), Some("mic-id"));
    assert_eq!(opts.audio.mic_volume, 1.5);
    assert_eq!(opts.audio.mic_channels, AudioChannelMode::Stereo);
}

#[test]
fn load_audio_split_toggle_from_json() {
    let json = r#"{
            "audio": {
                "split_output_by_process": false
            }
        }"#;
    let settings = AppSettings::load_from_object(
        serde_json::from_str::<Value>(json)
            .unwrap()
            .as_object()
            .unwrap(),
    );

    assert!(!settings.audio.split_output_by_process);
}

#[test]
fn service_options_include_video_encoder_choice() {
    let settings = AppSettings {
        video_encoder: VideoEncoder::AmfH264,
        ..AppSettings::default()
    };

    let opts = settings.to_service_options(None).unwrap();

    assert_eq!(opts.video_encoder, VideoEncoder::AmfH264);
}

#[test]
fn service_options_include_capture_backend_choice() {
    let settings = AppSettings {
        capture_backend: CaptureBackend::DesktopDuplication,
        ..AppSettings::default()
    };

    let opts = settings.to_service_options(None).unwrap();

    assert_eq!(opts.capture_backend, CaptureBackend::DesktopDuplication);
}

#[test]
fn capture_backend_defaults_to_auto() {
    assert_eq!(AppSettings::default().capture_backend, CaptureBackend::Auto);
}

#[test]
fn service_options_include_output_resolution_choice() {
    let settings = AppSettings {
        output_resolution: OutputResolution::P720,
        video_quality: VideoQuality::Sharp,
        ..AppSettings::default()
    };

    let opts = settings.to_service_options(None).unwrap();

    assert_eq!(opts.output_resolution, OutputResolution::P720);
    assert_eq!(opts.bitrate_bps, 8_000_000);
}

#[test]
fn advanced_recording_overrides_preset_service_values() {
    let settings = AppSettings {
        advanced_recording: AdvancedRecordingSettings {
            enabled: true,
            output_width: 1600,
            output_height: 900,
            bitrate_mbps: 13.5,
            fps: 75,
        },
        output_resolution: OutputResolution::P720,
        video_quality: VideoQuality::Compact,
        bitrate_mbps: 2.5,
        fps: 30,
        ..AppSettings::default()
    };

    let opts = settings.to_service_options(None).unwrap();
    let bounds = opts.output_resolution_bounds.unwrap();

    assert_eq!(opts.output_resolution, OutputResolution::P720);
    assert_eq!(bounds.width, 1600);
    assert_eq!(bounds.height, 900);
    assert_eq!(opts.bitrate_bps, 13_500_000);
    assert_eq!(opts.fps, 75);
}

#[test]
fn advanced_recording_load_repairs_numeric_values() {
    let value = serde_json::json!({
        "advanced_recording": {
            "enabled": true,
            "output_width": 1919,
            "output_height": 1079,
            "bitrate_mbps": 17.25,
            "fps": 75
        }
    });

    let settings = AppSettings::load_from_object(value.as_object().unwrap());

    assert!(settings.advanced_recording.enabled);
    assert_eq!(settings.advanced_recording.output_width, 1920);
    assert_eq!(settings.advanced_recording.output_height, 1080);
    assert_eq!(settings.advanced_recording.bitrate_mbps, 17.25);
    assert_eq!(settings.advanced_recording.fps, 75);
}

#[test]
fn advanced_recording_load_repairs_tiny_dimensions_to_encoder_safe_min() {
    let value = serde_json::json!({
        "advanced_recording": {
            "enabled": true,
            "output_width": 320,
            "output_height": 180,
            "bitrate_mbps": 12.0,
            "fps": 60
        }
    });

    let settings = AppSettings::load_from_object(value.as_object().unwrap());

    assert_eq!(settings.advanced_recording.output_width, 640);
    assert_eq!(settings.advanced_recording.output_height, 360);
}

#[test]
fn service_options_include_display_region_source() {
    let settings = AppSettings {
        capture_mode: CaptureMode::DisplayRegion,
        capture_region: CaptureRegionSettings {
            display_id: Some(r"\\.\DISPLAY1".into()),
            x: 100,
            y: 50,
            width: 800,
            height: 450,
        },
        ..AppSettings::default()
    };

    let opts = settings.to_service_options(None).unwrap();

    assert_eq!(
        opts.capture_source,
        CaptureSource::DisplayRegion(CaptureRegion {
            display_id: Some(r"\\.\DISPLAY1".into()),
            x: 100,
            y: 50,
            width: 800,
            height: 450,
        })
    );
}

#[test]
fn settings_round_trip_json() {
    let dir = TestDir::new("clipline-settings", "round-trip");
    let path = dir.path().join("settings.json");
    let settings = AppSettings {
        video_quality: VideoQuality::Sharp,
        bitrate_mbps: 16.0,
        output_resolution: OutputResolution::P1080,
        hotkey: "Ctrl+Alt+F9".into(),
        close_to_tray: false,
        minimize_to_tray: true,
        update_channel: UpdateChannel::Nightly,
        games: GameSettings {
            auto_detect: true,
            pause_when_no_game: false,
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
                legacy_ids: Vec::new(),
                name: "Notepad".into(),
                enabled: true,
                exe_name: "notepad.exe".into(),
                process_path: Some(r"C:\Windows\System32\notepad.exe".into()),
                window_title: "Untitled - Notepad".into(),
                recording_mode: GameRecordingMode::FullSession,
                icon: None,
            }],
        },
        ..AppSettings::default()
    };

    settings.save_to(&path).unwrap();
    let loaded = AppSettings::load_from(&path).unwrap();

    assert_eq!(loaded, settings);
}

#[test]
fn validation_rejects_custom_game_without_match_identity() {
    let settings = AppSettings {
        games: GameSettings {
            auto_detect: true,
            pause_when_no_game: false,
            plugins: BTreeMap::new(),
            custom_games: vec![CustomGameSettings {
                id: "custom-empty".into(),
                legacy_ids: Vec::new(),
                name: "Mystery".into(),
                enabled: true,
                exe_name: " ".into(),
                process_path: None,
                window_title: " ".into(),
                recording_mode: GameRecordingMode::ReplaysOnly,
                icon: None,
            }],
        },
        ..AppSettings::default()
    };

    assert!(settings.validate().is_err());
}

#[test]
fn parses_alt_f10_hotkey() {
    assert_eq!(normalize_hotkey("alt+f10").unwrap(), "Alt+F10");
    assert!(parse_hotkey("Alt+F10").is_ok());
}

#[test]
fn parses_multi_modifier_hotkey() {
    assert_eq!(normalize_hotkey("ctrl+shift+f9").unwrap(), "Ctrl+Shift+F9");
    assert!(parse_hotkey("Ctrl+Shift+F9").is_ok());
}

#[test]
fn parses_modified_keyboard_hotkeys() {
    assert_eq!(normalize_hotkey("ctrl+g").unwrap(), "Ctrl+G");
    assert_eq!(normalize_hotkey("ctrl+f").unwrap(), "Ctrl+F");
    assert_eq!(
        normalize_hotkey("alt+shift+arrowleft").unwrap(),
        "Alt+Shift+ArrowLeft"
    );
    assert_eq!(normalize_hotkey("ctrl+1").unwrap(), "Ctrl+1");
    assert_eq!(normalize_hotkey("ctrl+space").unwrap(), "Ctrl+Space");
    assert_eq!(normalize_hotkey("ctrl+slash").unwrap(), "Ctrl+Slash");
}

#[test]
fn parses_mouse_button_hotkeys() {
    assert_eq!(normalize_hotkey("ctrl+mouse4").unwrap(), "Ctrl+Mouse4");
    assert_eq!(normalize_hotkey("alt+mouse5").unwrap(), "Alt+Mouse5");
    assert_eq!(normalize_hotkey("shift+middle").unwrap(), "Shift+Middle");
    assert_eq!(normalize_hotkey("mouse4").unwrap(), "Mouse4");
    assert_eq!(normalize_hotkey("Mouse5").unwrap(), "Mouse5");
    assert_eq!(normalize_hotkey("Middle").unwrap(), "Middle");
}

#[test]
fn rejects_non_function_key_hotkeys() {
    assert!(normalize_hotkey("S").is_err());
    assert!(normalize_hotkey("1").is_err());
    assert!(normalize_hotkey("Slash").is_err());
    assert!(parse_hotkey("F12").is_err());
}

#[test]
fn rejects_windows_reserved_hotkeys() {
    assert!(normalize_hotkey("Alt+Tab").is_err());
    assert!(normalize_hotkey("Alt+F4").is_err());
    assert!(normalize_hotkey("Ctrl+Alt+Delete").is_err());
    assert!(normalize_hotkey("Ctrl+Shift+Esc").is_err());
}

#[test]
fn rejects_unsupported_mouse_hotkeys() {
    assert!(normalize_hotkey("Mouse1").is_err());
    assert!(normalize_hotkey("RightMouse").is_err());
    assert!(normalize_hotkey("forward").is_err());
}

#[test]
fn parses_plain_function_key_hotkey() {
    assert_eq!(normalize_hotkey("f10").unwrap(), "F10");
    assert!(parse_hotkey("F10").is_ok());
}

#[test]
fn quota_zero_disables_quota_lock() {
    assert_eq!(quota_bytes_from_gb(0.0).unwrap(), None);
    assert_eq!(quota_bytes_from_gb(0.5).unwrap(), Some(512 * 1024 * 1024));
}

#[test]
fn buffer_estimate_scales_with_duration_and_bitrate() {
    let small = estimated_buffer_bytes(60.0, 8.0);
    let large = estimated_buffer_bytes(120.0, 16.0);

    assert!(small >= 64 * 1024 * 1024);
    assert!(large > small * 3);
}

#[test]
fn thirty_second_replay_has_buffer_slack_for_encoder_overshoot() {
    let settings = AppSettings {
        replay_window_s: 30.0,
        buffer_seconds: 45.0,
        bitrate_mbps: 5.0,
        ..AppSettings::default()
    };
    let opts = settings.to_service_options(None).unwrap();

    assert!(opts.buffer_bytes >= 64 * 1024 * 1024);
}

#[test]
fn service_options_ignore_stale_legacy_buffer_seconds() {
    // `buffer_seconds` is a persisted compatibility mirror. Runtime sizing and
    // retention must derive from the replay window even if a live value is
    // stale or outside the old validation range.
    let settings = AppSettings {
        replay_window_s: 30.0,
        buffer_seconds: 1.0,
        ..AppSettings::default()
    };

    let opts = settings.to_service_options(None).unwrap();

    assert_eq!(opts.replay_window_s, 30.0);
    assert_eq!(
        opts.buffer_bytes,
        estimated_buffer_bytes(30.0, settings.effective_bitrate_mbps())
    );
}

#[test]
fn five_second_replay_uses_exact_retention() {
    let settings = AppSettings {
        replay_window_s: 5.0,
        buffer_seconds: 5.0,
        ..AppSettings::default()
    };

    let opts = settings.to_service_options(None).unwrap();

    assert_eq!(opts.replay_window_s, 5.0);
    assert_eq!(
        opts.buffer_bytes,
        estimated_buffer_bytes(5.0, settings.effective_bitrate_mbps())
    );
}

#[test]
fn load_normalizes_legacy_buffer_seconds_to_replay_window() {
    let dir = TestDir::new("clipline-settings", "buffer-headroom");
    let path = dir.path().join("settings.json");
    std::fs::write(
        &path,
        serde_json::json!({
            "capture_mode": "primary_monitor",
            "window_title": "",
            "buffer_seconds": 120.0,
            "replay_window_s": 30.0,
            "bitrate_mbps": 12.0,
            "fps": 60,
            "video_encoder": "auto",
            "disk_quota_gb": 10.0,
            "media_dir": default_media_dir(),
            "hotkey": "Alt+F10"
        })
        .to_string(),
    )
    .unwrap();

    let settings = AppSettings::load_from(&path).unwrap();

    assert_eq!(settings.buffer_seconds, 30.0);
    assert_eq!(settings.replay_storage, ReplayStorageSettings::default());
}

#[test]
fn save_normalizes_legacy_buffer_seconds_to_replay_window() {
    let dir = TestDir::new("clipline-settings", "save-buffer-mirror");
    let path = dir.path().join("settings.json");
    let settings = AppSettings {
        replay_window_s: 30.0,
        buffer_seconds: f64::INFINITY,
        ..AppSettings::default()
    };

    settings.save_to(&path).unwrap();

    let saved: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(saved["buffer_seconds"], 30.0);
    assert_eq!(AppSettings::load_from(&path).unwrap().buffer_seconds, 30.0);
}

#[test]
fn save_to_replaces_settings_via_temp_file() {
    let dir = TestDir::new("clipline-settings", "atomic-save");
    let path = dir.path().join("settings.json");
    let tmp = dir.path().join("settings.json.tmp");
    std::fs::write(&path, "{}").unwrap();
    std::fs::write(&tmp, "stale").unwrap();

    AppSettings::default().save_to(&path).unwrap();

    assert!(!tmp.exists(), "save_to must not leave stale temp files");
    assert_eq!(
        AppSettings::load_from(&path).unwrap(),
        AppSettings::default()
    );
}

#[test]
fn temporary_settings_paths_are_unique_per_save_attempt() {
    let dir = TestDir::new("clipline-settings", "atomic-save-unique-temp");
    let path = dir.path().join("settings.json");

    let first = sibling_tmp_path(&path).unwrap();
    let second = sibling_tmp_path(&path).unwrap();

    assert_ne!(first, second);
    for tmp in [first, second] {
        let name = tmp.file_name().unwrap().to_string_lossy();
        assert!(name.starts_with("settings.json."));
        assert!(name.ends_with(".tmp"));
        assert_eq!(tmp.parent(), path.parent());
    }
}

#[test]
fn startup_defaults_quietly_only_when_primary_and_backup_are_missing() {
    let dir = TestDir::new("clipline-settings", "startup-first-run");
    let path = dir.path().join("settings.json");

    let loaded = AppSettings::load_for_startup_from(&path);

    assert_eq!(loaded.settings, AppSettings::default());
    assert!(loaded.warnings.is_empty());
    assert!(loaded.first_run);
}

#[test]
fn startup_with_saved_settings_is_not_a_first_run() {
    let dir = TestDir::new("clipline-settings", "startup-existing-install");
    let path = dir.path().join("settings.json");
    let settings = AppSettings {
        buffer_seconds: 30.0,
        replay_window_s: 30.0,
        ..AppSettings::default()
    };
    settings.save_to(&path).unwrap();

    let loaded = AppSettings::load_for_startup_from(&path);

    assert_eq!(loaded.settings, settings);
    assert!(loaded.warnings.is_empty());
    assert!(!loaded.first_run);
}

#[test]
fn save_preserves_previous_valid_settings_as_last_known_good_backup() {
    let dir = TestDir::new("clipline-settings", "last-known-good");
    let path = dir.path().join("settings.json");
    let backup = dir.path().join("settings.json.bak");
    let previous = AppSettings {
        close_to_tray: false,
        ..AppSettings::default()
    };
    let current = AppSettings {
        minimize_to_tray: true,
        ..previous.clone()
    };
    previous.save_to(&path).unwrap();

    current.save_to(&path).unwrap();

    assert_eq!(AppSettings::load_from(&path).unwrap(), current);
    assert_eq!(AppSettings::load_from(&backup).unwrap(), previous);
}

#[test]
fn startup_quarantines_invalid_primary_and_recovers_last_known_good_backup() {
    let dir = TestDir::new("clipline-settings", "recover-backup");
    let path = dir.path().join("settings.json");
    let recovered = AppSettings {
        close_to_tray: false,
        ..AppSettings::default()
    };
    recovered.save_to(&path).unwrap();
    AppSettings {
        minimize_to_tray: true,
        ..recovered.clone()
    }
    .save_to(&path)
    .unwrap();
    std::fs::write(&path, "{ definitely not JSON").unwrap();

    let loaded = AppSettings::load_for_startup_from(&path);

    assert_eq!(loaded.settings, recovered);
    assert!(!loaded.first_run);
    assert_eq!(loaded.warnings.len(), 1);
    assert!(loaded.warnings[0].contains("recovered"));
    assert!(!path.exists());
    assert!(std::fs::read_dir(dir.path()).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("settings.json.corrupt.")
    }));
}

#[test]
fn startup_quarantines_invalid_primary_before_using_visible_safe_defaults() {
    let dir = TestDir::new("clipline-settings", "recover-defaults");
    let path = dir.path().join("settings.json");
    std::fs::write(&path, "[]").unwrap();

    let loaded = AppSettings::load_for_startup_from(&path);

    assert_eq!(loaded.settings, AppSettings::default());
    assert!(!loaded.first_run);
    assert_eq!(loaded.warnings.len(), 1);
    assert!(loaded.warnings[0].contains("safe defaults"));
    assert!(loaded.warnings[0].contains("preserved"));
    assert!(!path.exists());
}

#[test]
fn startup_leaves_unreadable_primary_untouched_while_recovering_backup() {
    let dir = TestDir::new("clipline-settings", "unreadable-primary");
    let path = dir.path().join("settings.json");
    let backup = dir.path().join("settings.json.bak");
    std::fs::create_dir(&path).unwrap();
    let recovered = AppSettings {
        minimize_to_tray: true,
        ..AppSettings::default()
    };
    recovered.save_to(&backup).unwrap();

    let loaded = AppSettings::load_for_startup_from(&path);

    assert_eq!(loaded.settings, recovered);
    assert!(!loaded.first_run);
    assert_eq!(loaded.warnings.len(), 1);
    assert!(loaded.warnings[0].contains("left untouched"));
    assert!(path.is_dir());
}

#[test]
fn save_refuses_to_overwrite_invalid_existing_primary_or_backup() {
    let dir = TestDir::new("clipline-settings", "block-invalid-overwrite");
    let path = dir.path().join("settings.json");
    let backup = dir.path().join("settings.json.bak");
    let last_known_good = AppSettings {
        close_to_tray: false,
        ..AppSettings::default()
    };
    last_known_good.save_to(&backup).unwrap();
    std::fs::write(&path, "broken but recoverable").unwrap();

    let error = AppSettings::default().save_to(&path).unwrap_err();

    assert!(error.contains("refusing to overwrite"));
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "broken but recoverable"
    );
    assert_eq!(AppSettings::load_from(&backup).unwrap(), last_known_good);
}

#[test]
fn uploaded_processing_status_survives_cloud_settings_normalization() {
    let mut cloud = CloudSettings::default();
    cloud.uploads.insert(
        "clip".into(),
        CloudUploadRecord {
            local_clip_id: "clip".into(),
            path: "D:\\Videos\\clip.mp4".into(),
            remote_clip_id: Some("remote".into()),
            remote_url: Some("https://clips.example.com/clip/remote".into()),
            visibility: "unlisted".into(),
            upload_status: "uploaded_processing".into(),
            error: Some("processing is still pending".into()),
            updated_at_unix: 2,
        },
    );

    cloud.normalize();

    assert_eq!(
        cloud.uploads.get("clip").unwrap().upload_status,
        "uploaded_processing"
    );
    assert!(cloud.validate().is_ok());
}

#[test]
fn cloud_settings_clear_legacy_synthesized_owner_share_urls() {
    let mut cloud = CloudSettings {
        host_url: "https://api.clips.example.com".into(),
        public_url: Some("https://clips.example.com".into()),
        ..CloudSettings::default()
    };
    cloud.uploads.insert(
        "legacy-owner".into(),
        CloudUploadRecord {
            local_clip_id: "legacy-owner".into(),
            path: "D:\\Videos\\legacy-owner.mp4".into(),
            remote_clip_id: Some("remote-owner".into()),
            remote_url: Some("https://old-clips.example.com/clip/remote-owner".into()),
            visibility: "unlisted".into(),
            upload_status: "uploaded_public".into(),
            error: None,
            updated_at_unix: 2,
        },
    );
    cloud.uploads.insert(
        "canonical-share".into(),
        CloudUploadRecord {
            local_clip_id: "canonical-share".into(),
            path: "D:\\Videos\\canonical-share.mp4".into(),
            remote_clip_id: Some("remote-share".into()),
            remote_url: Some("https://clips.example.com/c/c_public".into()),
            visibility: "public".into(),
            upload_status: "uploaded_public".into(),
            error: None,
            updated_at_unix: 3,
        },
    );
    cloud.uploads.insert(
        "now-private".into(),
        CloudUploadRecord {
            local_clip_id: "now-private".into(),
            path: "D:\\Videos\\now-private.mp4".into(),
            remote_clip_id: Some("remote-private".into()),
            remote_url: Some("https://clips.example.com/c/c_stale".into()),
            visibility: "private".into(),
            upload_status: "uploaded_private".into(),
            error: None,
            updated_at_unix: 4,
        },
    );

    cloud.normalize();

    assert_eq!(cloud.uploads["legacy-owner"].remote_url, None);
    assert_eq!(cloud.uploads["now-private"].remote_url, None);
    assert_eq!(
        cloud.uploads["canonical-share"].remote_url.as_deref(),
        Some("https://clips.example.com/c/c_public")
    );
}

#[test]
fn cloud_settings_normalize_connected_display_name() {
    let mut cloud = CloudSettings {
        connected_display_name: Some("  Dain  ".into()),
        ..CloudSettings::default()
    };

    cloud.normalize();

    assert_eq!(cloud.connected_display_name.as_deref(), Some("Dain"));

    cloud.connected_display_name = Some("  ".into());
    cloud.normalize();

    assert_eq!(cloud.connected_display_name, None);
}

#[test]
fn osu_api_settings_round_trip_without_secret() {
    let settings = AppSettings {
        osu: OsuApiSettings {
            client_id: Some("61835".into()),
            user: Some("3426414".into()),
            credential_target: Some("Clipline osu!:61835:3426414".into()),
            credential_cleanup_targets: vec!["Clipline osu!:old".into()],
            last_connected_username: Some("Dain".into()),
        },
        ..AppSettings::default()
    };

    let json = serde_json::to_string(&settings).unwrap();
    assert!(
        !json.contains("client_secret"),
        "osu! client secret must not be serialized into settings.json"
    );
    let round_trip: AppSettings = serde_json::from_str(&json).unwrap();

    assert_eq!(round_trip.osu.client_id.as_deref(), Some("61835"));
    assert_eq!(round_trip.osu.user.as_deref(), Some("3426414"));
    assert_eq!(
        round_trip.osu.credential_target.as_deref(),
        Some("Clipline osu!:61835:3426414")
    );
    assert_eq!(
        round_trip.osu.credential_cleanup_targets,
        ["Clipline osu!:old"]
    );
    assert_eq!(
        round_trip.osu.last_connected_username.as_deref(),
        Some("Dain")
    );
}

#[test]
fn disk_replay_requires_acknowledgement_and_folder() {
    let mut settings = AppSettings {
        replay_storage: ReplayStorageSettings {
            mode: ReplayStorageMode::Disk,
            disk_dir: String::new(),
            disk_quota_gb: 2.0,
            disk_acknowledged: false,
        },
        ..AppSettings::default()
    };

    assert!(settings.validate().is_err());
    settings.replay_storage.disk_acknowledged = true;
    assert!(settings.validate().is_err());
    settings.replay_storage.disk_dir = std::env::temp_dir()
        .join("clipline-cache")
        .display()
        .to_string();
    assert!(settings.validate().is_ok());
}

#[test]
fn disk_replay_rejects_media_folder_overlap() {
    let media = std::env::temp_dir().join("clipline-media");
    let settings = AppSettings {
        media_dir: media.display().to_string(),
        replay_storage: ReplayStorageSettings {
            mode: ReplayStorageMode::Disk,
            disk_dir: media.join("cache").display().to_string(),
            disk_quota_gb: 2.0,
            disk_acknowledged: true,
        },
        ..AppSettings::default()
    };

    assert!(settings.validate().is_err());
}

#[test]
fn disk_replay_rejects_case_variant_media_folder_overlap() {
    let media = std::env::temp_dir().join("clipline-media-case");
    let media_dir = media.display().to_string().to_ascii_uppercase();
    let cache_dir = media
        .join("cache")
        .display()
        .to_string()
        .to_ascii_lowercase();
    let settings = AppSettings {
        media_dir,
        replay_storage: ReplayStorageSettings {
            mode: ReplayStorageMode::Disk,
            disk_dir: cache_dir,
            disk_quota_gb: 2.0,
            disk_acknowledged: true,
        },
        ..AppSettings::default()
    };

    assert!(settings.validate().is_err());
}

#[test]
fn disk_replay_rejects_verbatim_media_folder_overlap() {
    let media = std::env::temp_dir().join("clipline-media-verbatim");
    let media_dir = media.display().to_string();
    let cache_dir = format!(r"\\?\{}", media.join("cache").display());
    let settings = AppSettings {
        media_dir,
        replay_storage: ReplayStorageSettings {
            mode: ReplayStorageMode::Disk,
            disk_dir: cache_dir,
            disk_quota_gb: 2.0,
            disk_acknowledged: true,
        },
        ..AppSettings::default()
    };

    assert!(settings.validate().is_err());
}
