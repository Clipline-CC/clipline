//! Custom game detection. This layer only consumes visible window/process
//! metadata exposed by Win32; it never opens game memory or injects code.

use clipline_capture::windows::{enumerate_capturable_windows, CapturableWindow};

use crate::game_identity::GameIdentity;
use crate::game_plugins::{self, GamePluginInfo};
use crate::settings::{CustomGameSettings, GameRecordingMode, GameSettings};

use std::sync::Mutex;
use std::time::Duration;

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
    let mut steam = steam_detector_state();
    let steam = steam.get_or_insert_with(SteamDetectorState::live);
    detect_active_game_from_windows_with_steam(settings, windows, steam)
}

pub(crate) fn detect_active_game_from_windows_with_steam(
    settings: &GameSettings,
    windows: Vec<CapturableWindow>,
    steam: &mut SteamDetectorState,
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
    if settings.auto_detect_steam_launches {
        return detect_steam_game_from_windows(windows, steam);
    }
    None
}

/// Upper bound on Steam manifest rescans while a Steam-rooted window misses
/// the catalog. Refreshes only ever happen on such a miss, never on the
/// plain 500 ms tick.
const STEAM_CATALOG_REFRESH_INTERVAL: Duration = Duration::from_secs(30);

/// Detector-owned Steam catalog cache. `live` states lazily scan the disk;
/// `fixed` states are injected test fixtures and never touch the filesystem.
pub(crate) struct SteamDetectorState {
    catalog: Option<crate::game_discovery::SteamLaunchCatalog>,
    live: bool,
}

impl SteamDetectorState {
    fn live() -> Self {
        Self {
            catalog: None,
            live: true,
        }
    }

    #[cfg(test)]
    pub(crate) fn fixed(catalog: crate::game_discovery::SteamLaunchCatalog) -> Self {
        Self {
            catalog: Some(catalog),
            live: false,
        }
    }

    fn catalog_mut(&mut self) -> &mut crate::game_discovery::SteamLaunchCatalog {
        if self.catalog.is_none() && self.live {
            self.catalog = Some(crate::game_discovery::SteamLaunchCatalog::scan());
        }
        self.catalog.get_or_insert_with(|| crate::game_discovery::SteamLaunchCatalog {
            apps: Vec::new(),
            common_roots: Vec::new(),
            loaded_at: std::time::Instant::now(),
        })
    }
}

// ponytail: one shared cache for the single detector thread; move into
// spawn_game_detector state if a second detector ever exists.
static STEAM_DETECTOR_STATE: Mutex<Option<SteamDetectorState>> = Mutex::new(None);

fn steam_detector_state() -> std::sync::MutexGuard<'static, Option<SteamDetectorState>> {
    STEAM_DETECTOR_STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn detect_steam_game_from_windows(
    windows: Vec<CapturableWindow>,
    steam: &mut SteamDetectorState,
) -> Option<DetectedGame> {
    // Cheap gate before touching the catalog: a Steam install path always
    // contains `steamapps\common`, so non-Steam ticks stay off the disk.
    let mut candidates: Vec<_> = windows
        .into_iter()
        .filter(|window| {
            window
                .exe_path
                .as_deref()
                .is_some_and(|path| path.to_ascii_lowercase().contains(r"steamapps\common"))
        })
        .collect();
    if candidates.is_empty() {
        return None;
    }
    // A window any registered plugin would match stays plugin-owned even
    // while that plugin is disabled, so this path can never revive it.
    for plugin in game_plugins::all() {
        let matched_handle = plugin.match_window(&candidates).map(|window| window.handle);
        if let Some(handle) = matched_handle {
            candidates.retain(|window| window.handle != handle);
        }
    }
    candidates.retain(|window| !crate::game_discovery::is_noise_window(window));

    let live = steam.live;
    let catalog = steam.catalog_mut();
    let mut matched = find_best_steam_match(catalog, &candidates);
    if matched.is_none()
        && live
        && catalog.loaded_at.elapsed() >= STEAM_CATALOG_REFRESH_INTERVAL
        && candidates.iter().any(|window| {
            window
                .exe_path
                .as_deref()
                .is_some_and(|path| catalog.is_steam_rooted(path))
        })
    {
        // Refresh only on a Steam-rooted miss so a browser or Explorer
        // window can never trigger a manifest scan, and at most once per
        // interval. A Steam library added after the first scan stays
        // unnoticed until restart; rescan on a timer if that ever matters.
        *catalog = crate::game_discovery::SteamLaunchCatalog::scan();
        matched = find_best_steam_match(catalog, &candidates);
    }

    let (app, window) = matched?;
    let name = if app.name.trim().is_empty() {
        window
            .exe_name
            .trim()
            .strip_suffix(".exe")
            .unwrap_or(window.exe_name.trim())
            .to_owned()
    } else {
        app.name.clone()
    };
    Some(DetectedGame {
        identity: GameIdentity::discovered_steam(app.app_id),
        name,
        hwnd: window.handle,
        window_title: window.title.clone(),
        process_id: window.process_id,
        exe_name: window.exe_name.clone(),
        exe_path: window.exe_path.clone(),
        recording_mode: GameRecordingMode::ReplaysOnly,
    })
}

fn find_best_steam_match<'a>(
    catalog: &'a crate::game_discovery::SteamLaunchCatalog,
    candidates: &'a [CapturableWindow],
) -> Option<(&'a crate::game_discovery::SteamLaunchApp, &'a CapturableWindow)> {
    candidates
        .iter()
        .filter_map(|window| {
            let path = window.exe_path.as_deref()?;
            catalog.find_by_exe_path(path).map(|app| (app, window))
        })
        // Several windows can share one Steam exe (game + splash); the
        // longest title is the actual gameplay window.
        .max_by_key(|(_, window)| window.title.len())
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
        && (settings.auto_detect_steam_launches
            || game_plugins::all()
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
            auto_detect_steam_launches: false,
            plugins: settings_with_all_plugins_disabled().plugins,
            custom_games: Vec::new(),
        }));
        assert!(!has_enabled_games(&GameSettings {
            auto_detect: true,
            pause_when_no_game: false,
            auto_detect_steam_launches: false,
            plugins: settings_with_all_plugins_disabled().plugins,
            custom_games: vec![CustomGameSettings {
                enabled: false,
                ..game()
            }],
        }));
        assert!(
            has_enabled_games(&GameSettings {
                auto_detect: true,
                custom_games: Vec::new(),
                ..GameSettings::default()
            }),
            "Steam launch detection alone must count as an enabled source"
        );
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
            auto_detect_steam_launches: false,
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

    fn steam_catalog() -> crate::game_discovery::SteamLaunchCatalog {
        crate::game_discovery::SteamLaunchCatalog {
            apps: vec![crate::game_discovery::SteamLaunchApp {
                app_id: 427520,
                name: "Friendslop".into(),
                install_dir: std::path::PathBuf::from(r"C:\Steam\steamapps\common\Friendslop"),
            }],
            common_roots: vec![std::path::PathBuf::from(r"C:\Steam\steamapps\common")],
            loaded_at: std::time::Instant::now(),
        }
    }

    fn detect_with_catalog(
        settings: &GameSettings,
        windows: Vec<CapturableWindow>,
    ) -> Option<DetectedGame> {
        let mut steam = super::SteamDetectorState::fixed(steam_catalog());
        super::detect_active_game_from_windows_with_steam(settings, windows, &mut steam)
    }

    #[test]
    fn unmatched_steam_window_becomes_replays_only_discovered_game() {
        let detected = detect_with_catalog(
            &GameSettings::default(),
            vec![window(
                11,
                "Friendslop",
                "Friendslop.exe",
                Some(r"C:\Steam\steamapps\common\Friendslop\Friendslop.exe"),
            )],
        )
        .expect("unlisted Steam install should be detected");

        assert_eq!(detected.identity.id(), "steam-427520");
        assert_eq!(detected.name, "Friendslop");
        assert_eq!(detected.hwnd, 11);
        assert_eq!(detected.recording_mode, GameRecordingMode::ReplaysOnly);
        assert_eq!(
            detected.exe_path.as_deref(),
            Some(r"C:\Steam\steamapps\common\Friendslop\Friendslop.exe")
        );
    }

    #[test]
    fn built_in_plugin_still_wins_over_steam_path() {
        let detected = detect_with_catalog(
            &GameSettings::default(),
            vec![
                window(
                    2,
                    "League of Legends (TM) Client",
                    "League of Legends.exe",
                    Some(r"C:\Steam\steamapps\common\League of Legends\League of Legends.exe"),
                ),
                window(
                    1,
                    "Friendslop",
                    "Friendslop.exe",
                    Some(r"C:\Steam\steamapps\common\Friendslop\Friendslop.exe"),
                ),
            ],
        )
        .expect("League window should match first");

        assert_eq!(
            detected.identity.id(),
            crate::game_plugins::LEAGUE_OF_LEGENDS_ID
        );
        assert_eq!(detected.hwnd, 2);
    }

    #[test]
    fn disabled_plugin_window_is_not_revived_by_steam_path() {
        // A League-shaped window that happens to live under a Steam library
        // must stay plugin-owned even while the plugin is disabled.
        let settings = GameSettings {
            auto_detect: true,
            pause_when_no_game: false,
            auto_detect_steam_launches: true,
            plugins: settings_with_league(false, GameRecordingMode::FullSession).plugins,
            custom_games: Vec::new(),
        };
        let catalog = crate::game_discovery::SteamLaunchCatalog {
            apps: vec![crate::game_discovery::SteamLaunchApp {
                app_id: 1,
                name: "League of Legends".into(),
                install_dir: std::path::PathBuf::from(
                    r"C:\Steam\steamapps\common\League of Legends",
                ),
            }],
            common_roots: vec![std::path::PathBuf::from(r"C:\Steam\steamapps\common")],
            loaded_at: std::time::Instant::now(),
        };
        let mut steam = super::SteamDetectorState::fixed(catalog);

        assert!(super::detect_active_game_from_windows_with_steam(
            &settings,
            vec![window(
                7,
                "League of Legends (TM) Client",
                "League of Legends.exe",
                Some(r"C:\Steam\steamapps\common\League of Legends\League of Legends.exe"),
            )],
            &mut steam,
        )
        .is_none());
    }

    #[test]
    fn enabled_custom_game_still_wins_over_steam_path() {
        let settings = GameSettings {
            auto_detect: true,
            custom_games: vec![CustomGameSettings {
                id: "custom-friendslop".into(),
                exe_name: "Friendslop.exe".into(),
                process_path: Some(
                    r"C:\Steam\steamapps\common\Friendslop\Friendslop.exe".into(),
                ),
                window_title: "Friendslop".into(),
                recording_mode: GameRecordingMode::FullSession,
                ..game()
            }],
            ..GameSettings::default()
        };

        let detected = detect_with_catalog(
            &settings,
            vec![window(
                3,
                "Friendslop",
                "Friendslop.exe",
                Some(r"C:\Steam\steamapps\common\Friendslop\Friendslop.exe"),
            )],
        )
        .expect("enabled custom rule should win");

        assert_eq!(detected.identity.id(), "custom-friendslop");
        assert_eq!(detected.recording_mode, GameRecordingMode::FullSession);
    }

    #[test]
    fn steam_helpers_launchers_and_client_are_ignored() {
        for (title, exe, path) in [
            (
                "Friendslop Crash",
                "UnityCrashHandler64.exe",
                r"C:\Steam\steamapps\common\Friendslop\UnityCrashHandler64.exe",
            ),
            (
                "Friendslop Launcher",
                "FriendslopLauncher.exe",
                r"C:\Steam\steamapps\common\Friendslop\FriendslopLauncher.exe",
            ),
            ("Steam", "steam.exe", r"C:\Steam\steam.exe"),
            (
                "Friendslop - Store",
                "steamwebhelper.exe",
                r"C:\Steam\steamapps\common\Friendslop\steamwebhelper.exe",
            ),
        ] {
            assert!(
                detect_with_catalog(
                    &GameSettings::default(),
                    vec![window(4, title, exe, Some(path))],
                )
                .is_none(),
                "{exe} should be ignored as Steam noise"
            );
        }
    }

    #[test]
    fn steam_launch_detection_setting_and_master_switch_gate_the_match() {
        let windows = vec![window(
            5,
            "Friendslop",
            "Friendslop.exe",
            Some(r"C:\Steam\steamapps\common\Friendslop\Friendslop.exe"),
        )];

        let setting_off = GameSettings {
            auto_detect_steam_launches: false,
            ..GameSettings::default()
        };
        assert!(detect_with_catalog(&setting_off, windows.clone()).is_none());

        let master_off = GameSettings {
            auto_detect: false,
            ..GameSettings::default()
        };
        assert!(detect_with_catalog(&master_off, windows).is_none());
    }

    #[test]
    fn live_steam_catalog_initializes_from_a_real_scan() {
        if std::env::var_os("CI").is_some() {
            // Device test: proves the live init path scans this machine's
            // Steam instead of installing the empty stub that broke the
            // refresh gate. CI runners have no Steam install to observe.
            return;
        }
        let mut state = SteamDetectorState::live();
        assert!(state.catalog.is_none(), "construction stays lazy");
        let catalog = state.catalog_mut();
        assert!(
            !catalog.common_roots.is_empty() || !catalog.apps.is_empty(),
            "live init must populate the catalog via scan(), not an empty stub"
        );
    }

    #[test]
    fn path_under_install_dir_matches_even_when_exe_is_not_the_folder_exe() {
        // Unity/Unreal layout: the running exe is nested and named nothing
        // like the install folder. Lookup is install-dir prefix only.
        let detected = detect_with_catalog(
            &GameSettings::default(),
            vec![window(
                8,
                "Friendslop",
                "FriendslopGame.exe",
                Some(r"C:\Steam\steamapps\common\Friendslop\Friendslop\Binaries\Win64\FriendslopGame.exe"),
            )],
        )
        .expect("nested exe under the install dir should match");

        assert_eq!(detected.identity.id(), "steam-427520");
        assert_eq!(detected.name, "Friendslop");
    }

    #[test]
    fn longest_title_wins_when_windows_share_one_steam_exe() {
        let detected = detect_with_catalog(
            &GameSettings::default(),
            vec![
                window(
                    1,
                    "Friendslop",
                    "Friendslop.exe",
                    Some(r"C:\Steam\steamapps\common\Friendslop\Friendslop.exe"),
                ),
                window(
                    2,
                    "Friendslop - In Game (Ranked)",
                    "Friendslop.exe",
                    Some(r"C:\Steam\steamapps\common\Friendslop\Friendslop.exe"),
                ),
            ],
        )
        .expect("shared Steam exe should still pick a window");

        assert_eq!(detected.hwnd, 2);
    }
}
