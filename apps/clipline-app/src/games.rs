//! Custom game detection. This layer only consumes visible window/process
//! metadata exposed by Win32; it never opens game memory or injects code.

use clipline_capture::windows::{enumerate_capturable_windows, CapturableWindow};

use crate::game_identity::GameIdentity;
use crate::game_plugins::{self, GamePluginInfo};
use crate::settings::{CustomGameSettings, GameRecordingMode, GameSettings};

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct GameWindowInfo {
    pub title: String,
    pub process_id: u32,
    pub exe_name: String,
    pub exe_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedGame {
    pub identity: GameIdentity,
    pub name: String,
    pub hwnd: isize,
    pub window_title: String,
    pub process_id: u32,
    pub exe_name: String,
    pub exe_path: Option<String>,
    pub recording_mode: GameRecordingMode,
}

pub fn game_plugin_catalog() -> Vec<GamePluginInfo> {
    game_plugins::catalog()
}

pub fn list_game_windows() -> Vec<GameWindowInfo> {
    let current_pid = std::process::id();
    let mut windows: Vec<_> = enumerate_capturable_windows()
        .into_iter()
        .filter(|window| window.process_id != current_pid)
        .map(|window| GameWindowInfo {
            title: window.title,
            process_id: window.process_id,
            exe_name: window.exe_name,
            exe_path: window.exe_path,
        })
        .collect();
    windows.sort_by(|a, b| {
        a.exe_name
            .to_ascii_lowercase()
            .cmp(&b.exe_name.to_ascii_lowercase())
            .then_with(|| {
                a.title
                    .to_ascii_lowercase()
                    .cmp(&b.title.to_ascii_lowercase())
            })
    });
    windows
}

pub fn detect_active_game(settings: &GameSettings) -> Option<DetectedGame> {
    if !has_enabled_games(settings) {
        return None;
    }
    detect_active_game_from_windows(settings, enumerate_capturable_windows())
}

pub fn detect_active_game_from_windows(
    settings: &GameSettings,
    windows: Vec<CapturableWindow>,
) -> Option<DetectedGame> {
    if !settings.auto_detect {
        return None;
    }
    if let Some(game) = detect_built_in_game_from_windows(settings, &windows) {
        return Some(game);
    }
    for game in settings.custom_games.iter().filter(|game| game.enabled) {
        if let Some(window) = best_window_for_game(game, &windows) {
            return Some(DetectedGame {
                identity: GameIdentity::custom(game.id.clone()),
                name: game.name.clone(),
                hwnd: window.handle,
                window_title: window.title.clone(),
                process_id: window.process_id,
                exe_name: window.exe_name.clone(),
                exe_path: window.exe_path.clone(),
                recording_mode: game.recording_mode,
            });
        }
    }
    None
}

pub fn built_in_game_still_configured(settings: &GameSettings, identity: &GameIdentity) -> bool {
    let Some(id) = identity.plugin_id() else {
        return false;
    };
    settings.auto_detect
        && game_plugins::all()
            .iter()
            .find(|plugin| plugin.id() == id)
            .is_some_and(|plugin| plugin.settings(settings).enabled)
}

fn detect_built_in_game_from_windows(
    settings: &GameSettings,
    windows: &[CapturableWindow],
) -> Option<DetectedGame> {
    for plugin in game_plugins::all() {
        let plugin_settings = plugin.settings(settings);
        if !plugin_settings.enabled {
            continue;
        }
        if let Some(window) = plugin.match_window(windows) {
            // Opportunistically cache the icon for plugins that ship none —
            // a no-op for League (bundled) and once a cache exists.
            if let Some(path) = window.exe_path.as_deref() {
                game_plugins::ensure_plugin_icon_cached(plugin.id(), path);
            }
            return Some(DetectedGame {
                identity: GameIdentity::built_in_plugin(plugin.id())
                    .expect("registered game plugin id is reserved"),
                name: plugin.manifest.name.clone(),
                hwnd: window.handle,
                window_title: window.title.clone(),
                process_id: window.process_id,
                exe_name: window.exe_name.clone(),
                exe_path: window.exe_path.clone(),
                recording_mode: plugin_settings.recording_mode,
            });
        }
    }
    None
}

fn best_window_for_game<'a>(
    game: &CustomGameSettings,
    windows: &'a [CapturableWindow],
) -> Option<&'a CapturableWindow> {
    windows
        .iter()
        .filter_map(|window| match_score(game, window).map(|score| (score, window)))
        .max_by_key(|(score, window)| (*score, window.title.len()))
        .map(|(_, window)| window)
}

fn has_enabled_games(settings: &GameSettings) -> bool {
    settings.auto_detect
        && (game_plugins::all()
            .iter()
            .any(|plugin| plugin.settings(settings).enabled)
            || settings.custom_games.iter().any(|game| game.enabled))
}

fn match_score(game: &CustomGameSettings, window: &CapturableWindow) -> Option<u16> {
    let configured_path = game
        .process_path
        .as_deref()
        .filter(|path| !path.trim().is_empty());
    let configured_exe = (!game.exe_name.trim().is_empty()).then_some(game.exe_name.trim());
    let title_matches = !game.window_title.trim().is_empty()
        && contains_case_insensitive(&window.title, &game.window_title);

    if let Some(configured) = configured_path {
        if window
            .exe_path
            .as_deref()
            .is_some_and(|actual| path_key(configured) == path_key(actual))
        {
            return Some(if title_matches { 350 } else { 300 });
        }
        if window.exe_path.is_none()
            && configured_exe.is_some_and(|exe| exe.eq_ignore_ascii_case(window.exe_name.trim()))
        {
            return Some(if title_matches { 250 } else { 200 });
        }
        return None;
    }

    if let Some(exe) = configured_exe {
        return exe
            .eq_ignore_ascii_case(window.exe_name.trim())
            .then_some(if title_matches { 250 } else { 200 });
    }

    if title_matches && !is_browser_process(window) {
        return Some(100);
    }
    None
}

fn contains_case_insensitive(haystack: &str, needle: &str) -> bool {
    haystack
        .to_ascii_lowercase()
        .contains(&needle.trim().to_ascii_lowercase())
}

pub(crate) fn path_key(path: &str) -> String {
    let mut normalized = path.trim().replace('/', "\\").to_ascii_lowercase();
    while normalized.ends_with('\\') && !normalized.ends_with(":\\") {
        normalized.pop();
    }
    normalized
}

fn is_browser_process(window: &CapturableWindow) -> bool {
    matches!(
        window.exe_name.trim().to_ascii_lowercase().as_str(),
        "arc.exe"
            | "brave.exe"
            | "chrome.exe"
            | "firefox.exe"
            | "librewolf.exe"
            | "msedge.exe"
            | "opera.exe"
            | "vivaldi.exe"
            | "waterfox.exe"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn game() -> CustomGameSettings {
        CustomGameSettings {
            id: "custom-test".into(),
            legacy_ids: Vec::new(),
            name: "Test Game".into(),
            enabled: true,
            exe_name: "game.exe".into(),
            process_path: Some(r"C:\Games\Test\game.exe".into()),
            window_title: "Test Game".into(),
            recording_mode: Default::default(),
            icon: None,
        }
    }

    fn window(
        handle: isize,
        title: &str,
        exe_name: &str,
        exe_path: Option<&str>,
    ) -> CapturableWindow {
        CapturableWindow {
            handle,
            title: title.into(),
            process_id: handle as u32,
            exe_name: exe_name.into(),
            exe_path: exe_path.map(str::to_string),
        }
    }

    fn settings_with_league(enabled: bool, recording_mode: GameRecordingMode) -> GameSettings {
        let mut settings = GameSettings::default();
        settings.plugins.insert(
            crate::game_plugins::LEAGUE_OF_LEGENDS_ID.into(),
            crate::settings::GamePluginSettings {
                enabled,
                recording_mode,
                review: Default::default(),
            },
        );
        settings
    }

    fn settings_with_all_plugins_disabled() -> GameSettings {
        let mut settings = GameSettings::default();
        for plugin in crate::game_plugins::all() {
            settings.plugins.insert(
                plugin.id().into(),
                crate::settings::GamePluginSettings {
                    enabled: false,
                    recording_mode: plugin.manifest.default_recording_mode,
                    review: Default::default(),
                },
            );
        }
        settings
    }

    #[test]
    fn detects_first_enabled_custom_game_by_process_path() {
        let settings = GameSettings {
            auto_detect: true,
            custom_games: vec![CustomGameSettings {
                recording_mode: GameRecordingMode::FullSession,
                ..game()
            }],
            ..GameSettings::default()
        };
        let detected = detect_active_game_from_windows(
            &settings,
            vec![window(
                42,
                "Unexpected title",
                "game.exe",
                Some(r"c:/games/test/GAME.exe"),
            )],
        )
        .expect("game should match by path");

        assert_eq!(detected.hwnd, 42);
        assert_eq!(detected.name, "Test Game");
        assert_eq!(detected.recording_mode, GameRecordingMode::FullSession);
    }

    #[test]
    fn falls_back_to_exe_name_when_path_is_unavailable() {
        let settings = GameSettings {
            auto_detect: true,
            custom_games: vec![game()],
            ..GameSettings::default()
        };
        let detected = detect_active_game_from_windows(
            &settings,
            vec![window(7, "Different title", "GAME.EXE", None)],
        )
        .expect("game should match by executable name");

        assert_eq!(detected.hwnd, 7);
    }

    #[test]
    fn configured_custom_game_does_not_match_browser_tab_title() {
        let settings = GameSettings {
            auto_detect: true,
            custom_games: vec![CustomGameSettings {
                name: "Slay the Spire 2".into(),
                window_title: "Slay the Spire 2".into(),
                exe_name: "slay-the-spire-2.exe".into(),
                process_path: Some(r"C:\Games\Slay the Spire 2\slay-the-spire-2.exe".into()),
                ..game()
            }],
            ..GameSettings::default()
        };

        assert!(detect_active_game_from_windows(
            &settings,
            vec![window(
                9,
                "Slay the Spire 2 - Gameplay Trailer - YouTube",
                "chrome.exe",
                Some(r"C:\Program Files\Google\Chrome\Application\chrome.exe"),
            )],
        )
        .is_none());
    }

    #[test]
    fn title_only_custom_game_ignores_browser_windows() {
        let title_only = CustomGameSettings {
            exe_name: String::new(),
            process_path: None,
            window_title: "Slay the Spire 2".into(),
            ..game()
        };
        let settings = GameSettings {
            auto_detect: true,
            custom_games: vec![title_only],
            ..GameSettings::default()
        };

        assert!(detect_active_game_from_windows(
            &settings,
            vec![window(9, "Slay the Spire 2 - YouTube", "msedge.exe", None)],
        )
        .is_none());

        let detected = detect_active_game_from_windows(
            &settings,
            vec![window(10, "Slay the Spire 2", "unknown-game.exe", None)],
        )
        .expect("title-only custom games should still match non-browser windows");
        assert_eq!(detected.hwnd, 10);
    }

    #[test]
    fn disabled_or_global_off_games_do_not_match() {
        let disabled = CustomGameSettings {
            enabled: false,
            ..game()
        };
        let windows = vec![window(
            1,
            "Test Game",
            "game.exe",
            Some(r"C:\Games\Test\game.exe"),
        )];

        assert!(detect_active_game_from_windows(
            &GameSettings {
                auto_detect: true,
                custom_games: vec![disabled],
                ..GameSettings::default()
            },
            windows.clone(),
        )
        .is_none());
        assert!(detect_active_game_from_windows(
            &GameSettings {
                auto_detect: false,
                custom_games: vec![game()],
                ..GameSettings::default()
            },
            windows,
        )
        .is_none());
    }

    #[test]
    fn no_enabled_games_can_skip_window_enumeration() {
        assert!(!has_enabled_games(&GameSettings {
            auto_detect: true,
            pause_when_no_game: false,
            plugins: settings_with_all_plugins_disabled().plugins,
            custom_games: Vec::new(),
        }));
        assert!(!has_enabled_games(&GameSettings {
            auto_detect: true,
            pause_when_no_game: false,
            plugins: settings_with_all_plugins_disabled().plugins,
            custom_games: vec![CustomGameSettings {
                enabled: false,
                ..game()
            }],
        }));
        assert!(has_enabled_games(&GameSettings {
            auto_detect: true,
            custom_games: Vec::new(),
            ..GameSettings::default()
        }));
        assert!(has_enabled_games(&GameSettings {
            auto_detect: true,
            custom_games: vec![game()],
            ..GameSettings::default()
        }));
    }

    #[test]
    fn detects_league_in_game_window_as_built_in_full_session() {
        let detected = detect_active_game_from_windows(
            &GameSettings::default(),
            vec![
                window(1, "League of Legends", "LeagueClientUx.exe", None),
                window(
                    2,
                    "League of Legends (TM) Client",
                    "League of Legends.exe",
                    Some(r"C:\Riot Games\League of Legends\Game\League of Legends.exe"),
                ),
            ],
        )
        .expect("League game window should match");

        assert_eq!(
            detected.identity.id(),
            crate::game_plugins::LEAGUE_OF_LEGENDS_ID
        );
        assert_eq!(detected.name, "League of Legends");
        assert_eq!(detected.hwnd, 2);
        assert_eq!(
            detected.exe_path.as_deref(),
            Some(r"C:\Riot Games\League of Legends\Game\League of Legends.exe")
        );
        assert_eq!(detected.recording_mode, GameRecordingMode::FullSession);
    }

    #[test]
    fn detects_osu_window_as_built_in_full_session() {
        let detected = detect_active_game_from_windows(
            &GameSettings::default(),
            vec![
                window(1, "osu!", "osu!.exe", None),
                window(
                    2,
                    "osu! - camellia - exit this earth's atomosphere",
                    "osu!.exe",
                    Some(r"C:\Users\dain\AppData\Local\osu!\osu!.exe"),
                ),
            ],
        )
        .expect("osu! game window should match");

        assert_eq!(
            detected.identity.id(),
            crate::game_plugins::plugin_id_for_game_id(clipline_events::GameId::Osu)
        );
        assert_eq!(detected.name, "osu!");
        assert_eq!(detected.hwnd, 2);
        assert_eq!(detected.recording_mode, GameRecordingMode::FullSession);
    }

    #[test]
    fn detects_osu_cutting_edge_build_title_as_built_in_full_session() {
        let detected = detect_active_game_from_windows(
            &GameSettings::default(),
            vec![window(
                1,
                "osu!cuttingedge b20260624",
                "osu!.exe",
                Some(r"C:\Users\dain\AppData\Roaming\osu!\osu!.exe"),
            )],
        )
        .expect("osu! cutting-edge gameplay window should match");

        assert_eq!(
            detected.identity.id(),
            crate::game_plugins::plugin_id_for_game_id(clipline_events::GameId::Osu)
        );
        assert_eq!(detected.name, "osu!");
        assert_eq!(detected.hwnd, 1);
        assert_eq!(detected.recording_mode, GameRecordingMode::FullSession);
    }

    #[test]
    fn detects_osu_stable_play_title_with_extra_spacing_as_built_in_full_session() {
        let detected = detect_active_game_from_windows(
            &GameSettings::default(),
            vec![window(
                1,
                "osu!  - ginkiha - EOS [Lycoris]",
                "osu!.exe",
                Some(r"C:\Users\dain\AppData\Roaming\osu!\osu!.exe"),
            )],
        )
        .expect("osu! stable gameplay window should match");

        assert_eq!(
            detected.identity.id(),
            crate::game_plugins::plugin_id_for_game_id(clipline_events::GameId::Osu)
        );
        assert_eq!(detected.name, "osu!");
        assert_eq!(detected.hwnd, 1);
        assert_eq!(detected.recording_mode, GameRecordingMode::FullSession);
    }

    #[test]
    fn detects_osu_stable_idle_title_as_built_in_full_session() {
        let detected = detect_active_game_from_windows(
            &GameSettings::default(),
            vec![window(
                1,
                "osu!",
                "osu!.exe",
                Some(r"C:\Users\dain\AppData\Roaming\osu!\osu!.exe"),
            )],
        )
        .expect("osu! stable idle window should match");

        assert_eq!(
            detected.identity.id(),
            crate::game_plugins::plugin_id_for_game_id(clipline_events::GameId::Osu)
        );
        assert_eq!(detected.name, "osu!");
        assert_eq!(detected.hwnd, 1);
        assert_eq!(detected.recording_mode, GameRecordingMode::FullSession);
    }

    #[test]
    fn ignores_osu_updater_windows_as_built_in_sessions() {
        for title in [
            "osu! updater",
            "osu! cutting edge",
            "osu! update available",
            "osu! updating",
        ] {
            assert!(
                detect_active_game_from_windows(
                    &GameSettings::default(),
                    vec![window(
                        1,
                        title,
                        "osu!.exe",
                        Some(r"C:\Users\dain\AppData\Local\osu!\osu!.exe"),
                    )],
                )
                .is_none(),
                "{title:?} should not start an osu! full-session recording"
            );
        }
    }

    #[test]
    fn league_client_alone_does_not_count_as_in_game() {
        assert!(detect_active_game_from_windows(
            &GameSettings::default(),
            vec![window(1, "League of Legends", "LeagueClientUx.exe", None)],
        )
        .is_none());
    }

    #[test]
    fn disabling_built_in_league_allows_custom_rules_to_take_over() {
        let settings = GameSettings {
            auto_detect: true,
            pause_when_no_game: false,
            plugins: settings_with_league(false, GameRecordingMode::FullSession).plugins,
            custom_games: vec![game()],
        };

        let detected = detect_active_game_from_windows(
            &settings,
            vec![window(7, "Test Game", "game.exe", None)],
        )
        .expect("custom game should still match");

        assert_eq!(detected.identity.id(), "custom-test");
    }

    #[test]
    fn disabled_built_in_game_can_be_captured_by_an_intentional_custom_rule() {
        let mut settings = settings_with_all_plugins_disabled();
        settings.custom_games.push(CustomGameSettings {
            id: "custom-league-duplicate".into(),
            name: "League of Legends".into(),
            exe_name: "League of Legends.exe".into(),
            process_path: Some(
                r"C:\Riot Games\League of Legends\Game\League of Legends.exe".into(),
            ),
            window_title: "League of Legends (TM) Client".into(),
            ..game()
        });

        let detected = detect_active_game_from_windows(
            &settings,
            vec![window(
                7,
                "League of Legends (TM) Client",
                "League of Legends.exe",
                Some(r"C:\Riot Games\League of Legends\Game\League of Legends.exe"),
            )],
        )
        .expect("disabled built-in game should defer to the custom rule");

        assert_eq!(detected.identity.id(), "custom-league-duplicate");
    }

    #[test]
    fn league_plugin_uses_saved_recording_mode() {
        let detected = detect_active_game_from_windows(
            &settings_with_league(true, GameRecordingMode::ReplaysOnly),
            vec![window(
                2,
                "League of Legends (TM) Client",
                "League of Legends.exe",
                None,
            )],
        )
        .expect("League game window should match");

        assert_eq!(detected.recording_mode, GameRecordingMode::ReplaysOnly);
    }

    #[test]
    fn plugin_catalog_exposes_league_metadata() {
        let plugins = game_plugin_catalog();

        assert!(plugins.iter().any(|plugin| {
            plugin.id == crate::game_plugins::LEAGUE_OF_LEGENDS_ID
                && plugin.name == "League of Legends"
                && plugin.default_enabled
                && plugin.default_recording_mode == GameRecordingMode::FullSession
                && plugin.default_review == crate::settings::GamePluginReviewSettings::default()
                && plugin.event_markers
        }));
    }
}
