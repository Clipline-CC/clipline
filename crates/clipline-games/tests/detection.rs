use clipline_games::detection::{detect_active_game_from_windows, GameWindow};
use clipline_games::identity::LEAGUE_OF_LEGENDS_ID;
use clipline_settings::{CustomGameSettings, GameRecordingMode, GameSettings};

#[test]
fn built_in_profiles_keep_priority_over_custom_rules() {
    let mut settings = GameSettings::default();
    settings.custom_games.push(CustomGameSettings {
        id: "custom-league-window".into(),
        legacy_ids: Vec::new(),
        name: "Custom League".into(),
        enabled: true,
        exe_name: "League of Legends.exe".into(),
        process_path: None,
        window_title: "League of Legends".into(),
        recording_mode: GameRecordingMode::ReplaysOnly,
        icon: None,
    });

    let detected = detect_active_game_from_windows(
        &settings,
        vec![GameWindow {
            handle: 42,
            title: "League of Legends (TM) Client".into(),
            process_id: 7,
            exe_name: "League of Legends.exe".into(),
            exe_path: Some(r"C:\Games\League of Legends.exe".into()),
        }],
    )
    .expect("built-in profile must match");

    assert!(detected.identity.is_built_in_plugin(LEAGUE_OF_LEGENDS_ID));
    assert_eq!(detected.recording_mode, GameRecordingMode::FullSession);
}
