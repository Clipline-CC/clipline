use std::path::PathBuf;

use clipline_capture::windows::CapturableWindow;
use clipline_test_utils::TestDir;

use crate::settings::CustomGameSettings;

use super::*;

fn window(title: &str, exe_name: &str, exe_path: Option<&str>) -> CapturableWindow {
    window_with_pid(101, title, exe_name, exe_path)
}

fn window_with_pid(
    process_id: u32,
    title: &str,
    exe_name: &str,
    exe_path: Option<&str>,
) -> CapturableWindow {
    CapturableWindow {
        handle: process_id as isize,
        title: title.into(),
        process_id,
        exe_name: exe_name.into(),
        exe_path: exe_path.map(str::to_string),
    }
}

fn vdf_path(path: &std::path::Path) -> String {
    path.to_string_lossy().replace('\\', "\\\\")
}

fn steam_app(
    app_id: u32,
    name: &str,
    install_dir: &str,
    exe_name: &str,
    process_path: &str,
) -> SteamApp {
    SteamApp {
        app_id,
        name: name.into(),
        install_dir: PathBuf::from(install_dir),
        exe_name: Some(exe_name.into()),
        process_path: Some(PathBuf::from(process_path)),
    }
}

#[test]
fn parses_libraryfolders_paths_from_keyvalue_vdf() {
    let input = r#"
        "libraryfolders"
        {
            "0"
            {
                "path" "C:\\Program Files (x86)\\Steam"
                "apps"
                {
                    "570" "12345"
                }
            }
            "1"
            {
                "path" "D:\\SteamLibrary"
            }
            "2" "E:\\LegacySteamLibrary"
        }
    "#;

    let parsed = parse_vdf(input).expect("libraryfolders parses");
    assert_eq!(
        library_paths_from_vdf(&parsed),
        vec![
            PathBuf::from(r"C:\Program Files (x86)\Steam"),
            PathBuf::from(r"D:\SteamLibrary"),
            PathBuf::from(r"E:\LegacySteamLibrary"),
        ]
    );
}

#[test]
fn parses_appmanifest_core_fields() {
    let input = r#"
        "AppState"
        {
            "appid" "646570"
            "name" "Slay the Spire"
            "installdir" "SlayTheSpire"
            "StateFlags" "4"
        }
    "#;

    let parsed = parse_vdf(input).expect("appmanifest parses");
    let manifest = steam_app_from_manifest(&parsed).expect("manifest fields");

    assert_eq!(manifest.app_id, 646570);
    assert_eq!(manifest.name, "Slay the Spire");
    assert_eq!(manifest.install_dir_name, "SlayTheSpire");
}

#[test]
fn malformed_vdf_returns_error() {
    let err = parse_vdf(r#""libraryfolders" { "0" { "path" "C:\\Steam""#)
        .expect_err("unclosed object should fail");
    assert!(
        err.contains("unterminated object"),
        "unexpected parse error: {err}"
    );
}

#[test]
fn executable_score_adds_contains_bonus_for_exact_game_name() {
    let install_dir = PathBuf::from(r"C:\Games\InstallFolder");
    let exe_path = install_dir.join("Game Name.exe");

    assert_eq!(executable_score(&exe_path, &install_dir, "Game Name"), 145);
}

#[test]
fn helper_filter_keeps_real_crash_named_games() {
    assert!(!is_helper_exe_name("Crashlands.exe"));
    assert!(!is_helper_exe_name("CrashBandicoot.exe"));
    assert!(is_helper_exe_name("UnityCrashHandler64.exe"));
    assert!(is_helper_exe_name("crashpad_handler.exe"));
    assert!(is_helper_exe_name("SkyrimSELauncher.exe"));
}

#[test]
fn executable_inference_keeps_crash_named_game_exe() {
    let dir = TestDir::new("clipline-game-discovery", "crash-game-exe");
    let install_dir = dir.path().join("Crashlands");
    std::fs::create_dir_all(&install_dir).unwrap();
    std::fs::write(install_dir.join("UnityCrashHandler64.exe"), b"").unwrap();
    std::fs::write(install_dir.join("Crashlands.exe"), b"").unwrap();

    let exe = infer_executable_path(&install_dir, "Crashlands")
        .expect("crash-named game executable should be considered");

    assert_eq!(
        exe.file_name().and_then(|name| name.to_str()),
        Some("Crashlands.exe")
    );
}

#[test]
fn executable_inference_skips_launcher_exes() {
    let dir = TestDir::new("clipline-game-discovery", "launcher-exe");
    let install_dir = dir.path().join("SkyrimSE");
    std::fs::create_dir_all(&install_dir).unwrap();
    std::fs::write(install_dir.join("SkyrimSE.exe"), b"").unwrap();
    std::fs::write(install_dir.join("SkyrimSELauncher.exe"), b"").unwrap();

    let exe = infer_executable_path(&install_dir, "Skyrim Special Edition")
        .expect("game executable should be selected");

    assert_eq!(
        exe.file_name().and_then(|name| name.to_str()),
        Some("SkyrimSE.exe")
    );
}

#[test]
fn steam_catalog_reads_manifests_and_infers_best_executable() {
    let dir = TestDir::new("clipline-game-discovery", "steam-catalog");
    let steam_root = dir.path().join("Steam");
    let library = dir.path().join("Library");
    std::fs::create_dir_all(steam_root.join("steamapps")).unwrap();
    std::fs::create_dir_all(library.join("steamapps/common/SlayTheSpire")).unwrap();
    std::fs::write(
        steam_root.join("steamapps/libraryfolders.vdf"),
        format!(
            r#""libraryfolders" {{ "0" {{ "path" "{}" }} "1" {{ "path" "{}" }} }}"#,
            vdf_path(&steam_root),
            vdf_path(&library)
        ),
    )
    .unwrap();
    std::fs::write(
        library.join("steamapps/appmanifest_646570.acf"),
        r#""AppState" { "appid" "646570" "name" "Slay the Spire" "installdir" "SlayTheSpire" }"#,
    )
    .unwrap();
    std::fs::write(
        library.join("steamapps/common/SlayTheSpire/UnityCrashHandler64.exe"),
        b"",
    )
    .unwrap();
    std::fs::write(
        library.join("steamapps/common/SlayTheSpire/SlayTheSpire.exe"),
        b"",
    )
    .unwrap();

    let apps = steam_apps_from_roots(&[steam_root]).expect("steam scan");
    assert_eq!(apps.len(), 1);
    assert_eq!(apps[0].app_id, 646570);
    assert_eq!(apps[0].name, "Slay the Spire");
    assert_eq!(
        apps[0].install_dir,
        library.join("steamapps/common/SlayTheSpire")
    );
    assert_eq!(apps[0].exe_name.as_deref(), Some("SlayTheSpire.exe"));
}

#[test]
fn steam_catalog_skips_malformed_manifest_and_continues() {
    let dir = TestDir::new("clipline-game-discovery", "steam-malformed");
    let steam_root = dir.path().join("Steam");
    let app_dir = steam_root.join("steamapps/common/Factorio");
    std::fs::create_dir_all(&app_dir).unwrap();
    std::fs::write(
        steam_root.join("steamapps/libraryfolders.vdf"),
        format!(
            r#""libraryfolders" {{ "0" {{ "path" "{}" }} }}"#,
            vdf_path(&steam_root)
        ),
    )
    .unwrap();
    std::fs::write(
        steam_root.join("steamapps/appmanifest_bad.acf"),
        r#""AppState" { "appid" "#,
    )
    .unwrap();
    std::fs::write(
        steam_root.join("steamapps/appmanifest_427520.acf"),
        r#""AppState" { "appid" "427520" "name" "Factorio" "installdir" "Factorio" }"#,
    )
    .unwrap();
    std::fs::write(app_dir.join("factorio.exe"), b"").unwrap();

    let apps = steam_apps_from_roots(&[steam_root]).expect("steam scan");
    assert_eq!(
        apps.iter().map(|app| app.name.as_str()).collect::<Vec<_>>(),
        vec!["Factorio"]
    );
}

#[test]
fn steam_catalog_scans_root_when_libraryfolders_is_malformed() {
    let dir = TestDir::new("clipline-game-discovery", "steam-bad-libraryfolders");
    let steam_root = dir.path().join("Steam");
    let app_dir = steam_root.join("steamapps/common/Hades");
    std::fs::create_dir_all(&app_dir).unwrap();
    std::fs::write(
        steam_root.join("steamapps/libraryfolders.vdf"),
        r#""libraryfolders" { "0" { "path" "#,
    )
    .unwrap();
    std::fs::write(
        steam_root.join("steamapps/appmanifest_1145360.acf"),
        r#""AppState" { "appid" "1145360" "name" "Hades" "installdir" "Hades" }"#,
    )
    .unwrap();
    std::fs::write(app_dir.join("Hades.exe"), b"").unwrap();

    let apps = steam_apps_from_roots(&[steam_root]).expect("steam scan");
    assert_eq!(apps.len(), 1);
    assert_eq!(apps[0].name, "Hades");
    assert_eq!(apps[0].exe_name.as_deref(), Some("Hades.exe"));
}

#[test]
fn running_window_under_steam_install_upgrades_installed_candidate() {
    let steam = SteamApp {
        app_id: 646570,
        name: "Slay the Spire".into(),
        install_dir: PathBuf::from(r"C:\Steam\steamapps\common\SlayTheSpire"),
        exe_name: Some("SlayTheSpire.exe".into()),
        process_path: Some(PathBuf::from(
            r"C:\Steam\steamapps\common\SlayTheSpire\SlayTheSpire.exe",
        )),
    };
    let candidates = candidates_from_sources(
        vec![steam],
        vec![window(
            "Slay the Spire",
            "SlayTheSpire.exe",
            Some(r"C:\Steam\steamapps\common\SlayTheSpire\SlayTheSpire.exe"),
        )],
        &[],
        |_| None,
    );

    assert_eq!(candidates.len(), 1);
    assert_eq!(
        candidates[0].source,
        DetectedGameSource::SteamAndRunningWindow
    );
    assert_eq!(candidates[0].window_title, "Slay the Spire");
    assert_eq!(candidates[0].confidence, 95);
}

#[test]
fn ignores_running_non_steam_windows() {
    let candidates = candidates_from_sources(
        Vec::new(),
        vec![window(
            "FINAL FANTASY XIV",
            "ffxiv_dx11.exe",
            Some(r"D:\Games\FFXIV\ffxiv_dx11.exe"),
        )],
        &[],
        |_| None,
    );

    assert!(candidates.is_empty());
}

#[test]
fn running_window_under_steam_install_adds_missing_installed_candidate_path() {
    let steam = SteamApp {
        app_id: 427520,
        name: "Factorio".into(),
        install_dir: PathBuf::from(r"D:\Steam\steamapps\common\Factorio"),
        exe_name: None,
        process_path: None,
    };
    let candidates = candidates_from_sources(
        vec![steam],
        vec![window(
            "Factorio",
            "factorio.exe",
            Some(r"D:\Steam\steamapps\common\Factorio\bin\x64\factorio.exe"),
        )],
        &[],
        |_| None,
    );

    assert_eq!(candidates.len(), 1);
    assert_eq!(
        candidates[0].source,
        DetectedGameSource::SteamAndRunningWindow
    );
    assert_eq!(candidates[0].steam_app_id, Some(427520));
    assert_eq!(candidates[0].name, "Factorio");
    assert_eq!(candidates[0].exe_name, "factorio.exe");
    assert_eq!(
        candidates[0].process_path.as_deref(),
        Some(r"D:\Steam\steamapps\common\Factorio\bin\x64\factorio.exe")
    );
}

#[test]
fn dedupes_against_existing_custom_games() {
    let existing = CustomGameSettings {
        id: "custom-factorio".into(),
        legacy_ids: Vec::new(),
        name: "factorio".into(),
        enabled: true,
        exe_name: "factorio.exe".into(),
        process_path: Some(r"D:\Steam\steamapps\common\Factorio\bin\x64\factorio.exe".into()),
        window_title: "Factorio".into(),
        recording_mode: crate::settings::GameRecordingMode::ReplaysOnly,
        icon: None,
    };
    let candidates = candidates_from_sources(
        vec![steam_app(
            427520,
            "Factorio",
            r"D:\Steam\steamapps\common\Factorio",
            "factorio.exe",
            r"D:\Steam\steamapps\common\Factorio\bin\x64\factorio.exe",
        )],
        Vec::new(),
        &[existing],
        |_| None,
    );

    assert!(candidates.is_empty());
}

#[test]
fn keeps_candidate_when_existing_custom_game_has_same_exe_but_different_path() {
    let existing = CustomGameSettings {
        id: "custom-game-a".into(),
        legacy_ids: Vec::new(),
        name: "Game A".into(),
        enabled: true,
        exe_name: "game.exe".into(),
        process_path: Some(r"D:\Games\A\game.exe".into()),
        window_title: "Game A".into(),
        recording_mode: crate::settings::GameRecordingMode::ReplaysOnly,
        icon: None,
    };
    let candidates = candidates_from_sources(
        vec![steam_app(
            200,
            "Game B",
            r"E:\Games\B",
            "game.exe",
            r"E:\Games\B\game.exe",
        )],
        Vec::new(),
        &[existing],
        |_| None,
    );

    assert_eq!(candidates.len(), 1);
    assert_eq!(
        candidates[0].process_path.as_deref(),
        Some(r"E:\Games\B\game.exe")
    );
}

#[test]
fn keeps_candidate_when_existing_custom_game_has_same_name_but_different_path() {
    let existing = CustomGameSettings {
        id: "custom-hades-epic".into(),
        legacy_ids: Vec::new(),
        name: "Hades".into(),
        enabled: true,
        exe_name: "Hades.exe".into(),
        process_path: Some(r"D:\Epic\Hades\Hades.exe".into()),
        window_title: "Hades".into(),
        recording_mode: crate::settings::GameRecordingMode::ReplaysOnly,
        icon: None,
    };
    let candidates = candidates_from_sources(
        vec![steam_app(
            1145360,
            "Hades",
            r"E:\Steam\steamapps\common\Hades",
            "Hades.exe",
            r"E:\Steam\steamapps\common\Hades\Hades.exe",
        )],
        Vec::new(),
        &[existing],
        |_| None,
    );

    assert_eq!(candidates.len(), 1);
    assert_eq!(
        candidates[0].process_path.as_deref(),
        Some(r"E:\Steam\steamapps\common\Hades\Hades.exe")
    );
}

#[test]
fn keeps_discovered_candidates_with_same_exe_but_different_paths() {
    let candidates = candidates_from_sources(
        vec![
            steam_app(
                100,
                "Game A",
                r"D:\Games\A",
                "game.exe",
                r"D:\Games\A\game.exe",
            ),
            steam_app(
                200,
                "Game B",
                r"E:\Games\B",
                "game.exe",
                r"E:\Games\B\game.exe",
            ),
        ],
        Vec::new(),
        &[],
        |_| None,
    );

    let mut paths = candidates
        .iter()
        .filter_map(|candidate| candidate.process_path.as_deref())
        .collect::<Vec<_>>();
    paths.sort_unstable();
    assert_eq!(paths, vec![r"D:\Games\A\game.exe", r"E:\Games\B\game.exe"]);
}

#[test]
fn still_dedupes_existing_custom_game_by_exe_when_existing_path_is_missing() {
    let existing = CustomGameSettings {
        id: "custom-game".into(),
        legacy_ids: Vec::new(),
        name: "Configured Game".into(),
        enabled: true,
        exe_name: "game.exe".into(),
        process_path: None,
        window_title: "Configured Game".into(),
        recording_mode: crate::settings::GameRecordingMode::ReplaysOnly,
        icon: None,
    };
    let candidates = candidates_from_sources(
        vec![steam_app(
            200,
            "Detected Game",
            r"E:\Games\B",
            "game.exe",
            r"E:\Games\B\game.exe",
        )],
        Vec::new(),
        &[existing],
        |_| None,
    );

    assert!(candidates.is_empty());
}
