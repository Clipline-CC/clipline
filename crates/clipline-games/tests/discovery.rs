use std::path::PathBuf;

use clipline_games::detection::GameWindow;
use clipline_games::discovery::{
    discovery_fields_match_custom_game, project_discovery_sources, DetectedGameSource,
    InstalledGameSource,
};
use clipline_settings::{CustomGameSettings, GameRecordingMode};

#[test]
fn installed_and_running_sources_merge_without_platform_access() {
    let executable = PathBuf::from(r"C:\Steam\steamapps\common\Example\example.exe");
    let candidates = project_discovery_sources(
        vec![InstalledGameSource {
            app_id: 123,
            name: "Example Game".into(),
            install_dir: PathBuf::from(r"C:\Steam\steamapps\common\Example"),
            exe_name: Some("example.exe".into()),
            process_path: Some(executable.clone()),
        }],
        vec![GameWindow {
            handle: 4,
            title: "Example Game - Playing".into(),
            process_id: 99,
            exe_name: "example.exe".into(),
            exe_path: Some(executable.to_string_lossy().into_owned()),
        }],
        &[],
    );

    assert_eq!(candidates.len(), 1);
    assert_eq!(
        candidates[0].source,
        DetectedGameSource::SteamAndRunningWindow
    );
    assert_eq!(candidates[0].steam_app_id, Some(123));
    assert_eq!(candidates[0].window_title, "Example Game - Playing");
}

#[test]
fn presentation_reuses_discovery_candidate_matching_without_rebuilding_rows() {
    let custom = CustomGameSettings {
        id: "custom-example".into(),
        legacy_ids: Vec::new(),
        name: "Example Game".into(),
        enabled: true,
        exe_name: "example.exe".into(),
        process_path: Some(r"C:\Games\Example\example.exe".into()),
        window_title: String::new(),
        recording_mode: GameRecordingMode::ReplaysOnly,
        icon: None,
    };

    assert!(discovery_fields_match_custom_game(
        Some("c:/games/example/example.exe"),
        "other.exe",
        "Other",
        &custom,
    ));
    assert!(discovery_fields_match_custom_game(
        None,
        "EXAMPLE.EXE",
        "Other",
        &custom,
    ));
    assert!(discovery_fields_match_custom_game(
        None,
        "",
        "Example-Game",
        &custom,
    ));
    assert!(!discovery_fields_match_custom_game(
        None,
        "other.exe",
        "Other",
        &custom,
    ));
}
