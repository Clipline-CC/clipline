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
    // The Steam preference only lasts while the fallback owns the capture;
    // a built-in or custom game taking over (or detection turning off)
    // clears it so it cannot resurrect an old Steam window later.
    if !settings.auto_detect {
        steam.current_app_id = None;
        return None;
    }
    if let Some(game) = detect_built_in_game_from_windows(settings, &windows) {
        steam.current_app_id = None;
        return Some(game);
    }
    for game in settings.custom_games.iter().filter(|game| game.enabled) {
        if let Some(window) = best_window_for_game(game, &windows) {
            steam.current_app_id = None;
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
        // A disabled custom rule still owns its windows: the Steam fallback
        // must not revive a game the user turned off (mirrors the plugin skip).
        let steam_windows: Vec<_> = windows
            .into_iter()
            .filter(|window| {
                !settings
                    .custom_games
                    .iter()
                    .filter(|game| !game.enabled)
                    .any(|game| disabled_custom_rule_owns_window(game, window))
            })
            .collect();
        return detect_steam_game_from_windows(steam_windows, steam, settings);
    }
    steam.current_app_id = None;
    None
}

/// A disabled rule owns a window its path names, or, for a path-less rule,
/// its exact exe name. Title substrings are too fuzzy to claim a window: they
/// could suppress an unrelated Steam game.
fn disabled_custom_rule_owns_window(game: &CustomGameSettings, window: &CapturableWindow) -> bool {
    match game
        .process_path
        .as_deref()
        .filter(|path| !path.trim().is_empty())
    {
        Some(configured) => window
            .exe_path
            .as_deref()
            .is_some_and(|actual| path_key(configured) == path_key(actual)),
        None => {
            let exe = game.exe_name.trim();
            !exe.is_empty() && exe.eq_ignore_ascii_case(window.exe_name.trim())
        }
    }
}

/// Upper bound on Steam manifest rescans while a Steam-rooted window misses
/// the catalog. Refreshes only ever happen on such a miss, never on the
/// plain 500 ms tick.
const STEAM_CATALOG_REFRESH_INTERVAL: Duration = Duration::from_secs(30);
/// Ceiling for the no-change refresh backoff so a permanently unmatched
/// Steam-rooted window still costs at most one rescan every 10 minutes.
const STEAM_CATALOG_REFRESH_MAX_BACKOFF: Duration = Duration::from_secs(600);

/// Detector-owned Steam catalog cache. `live` states lazily scan the disk;
/// `fixed` states are injected test fixtures and never touch the filesystem.
pub(crate) struct SteamDetectorState {
    catalog: Option<crate::game_discovery::SteamLaunchCatalog>,
    live: bool,
    refresh_wait: Duration,
    /// The Steam app matched on the previous tick. It keeps the capture while
    /// it still has a window, so alt-tabbing between two Steam apps does
    /// not flip detection (and restart the recorder) on every z-order change.
    current_app_id: Option<u32>,
    /// `path_key`s left unmatched by the last rescan; see `refresh_wait_for`.
    unmatched_at_last_refresh: Vec<String>,
}

impl SteamDetectorState {
    fn live() -> Self {
        Self {
            catalog: None,
            live: true,
            refresh_wait: STEAM_CATALOG_REFRESH_INTERVAL,
            current_app_id: None,
            unmatched_at_last_refresh: Vec::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn fixed(catalog: crate::game_discovery::SteamLaunchCatalog) -> Self {
        Self {
            catalog: Some(catalog),
            live: false,
            refresh_wait: STEAM_CATALOG_REFRESH_INTERVAL,
            current_app_id: None,
            unmatched_at_last_refresh: Vec::new(),
        }
    }

    fn catalog_mut(&mut self) -> &mut crate::game_discovery::SteamLaunchCatalog {
        if self.catalog.is_none() && self.live {
            self.catalog = Some(crate::game_discovery::SteamLaunchCatalog::scan());
        }
        self.catalog.get_or_insert_with(|| crate::game_discovery::SteamLaunchCatalog {
            apps: Vec::new(),
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
    settings: &GameSettings,
) -> Option<DetectedGame> {
    // Cheap gate before touching the catalog: a Steam install path always
    // contains `steamapps\common`, so non-Steam ticks stay off the disk.
    let mut candidates: Vec<_> = windows
        .into_iter()
        .filter(|window| {
            window
                .exe_path
                .as_deref()
                .is_some_and(|path| {
                    let lower = path.to_ascii_lowercase();
                    lower.contains(r"steamapps\common") || lower.contains("steamapps/common")
                })
        })
        .collect();
    if candidates.is_empty() {
        steam.current_app_id = None;
        return None;
    }
    for plugin in game_plugins::all() {
        while let Some(window) = plugin.match_window(&candidates) {
            let handle = window.handle;
            candidates.retain(|w| w.handle != handle);
        }
    }
    candidates.retain(|window| !crate::game_discovery::is_noise_window(window));

    maybe_refresh_steam_catalog(steam, &candidates);
    let preferred = steam.current_app_id;
    let detected = find_best_steam_match(steam.catalog_mut(), &candidates, preferred, settings).map(
        |(app, window)| {
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
            (
                app.app_id,
                DetectedGame {
                    identity: GameIdentity::discovered_steam(app.app_id),
                    name,
                    hwnd: window.handle,
                    window_title: window.title.clone(),
                    process_id: window.process_id,
                    exe_name: window.exe_name.clone(),
                    exe_path: window.exe_path.clone(),
                    recording_mode: GameRecordingMode::ReplaysOnly,
                },
            )
        },
    );
    steam.current_app_id = detected.as_ref().map(|(app_id, _)| *app_id);
    detected.map(|(_, game)| game)
}

/// Exe paths (as `path_key`s) of Steam-path windows no catalog app claims.
/// Candidates already passed the `steamapps\common` gate, so a browser or
/// Explorer window can never trigger a manifest scan. The catalog's own
/// roots are deliberately not required: a library added (or Steam
/// installed) after the first scan is exactly the case a rescan must find.
fn unmatched_steam_paths(
    catalog: &crate::game_discovery::SteamLaunchCatalog,
    candidates: &[CapturableWindow],
) -> Vec<String> {
    let mut paths: Vec<String> = candidates
        .iter()
        .filter_map(|window| window.exe_path.as_deref())
        .filter(|path| catalog.find_by_exe_path(path).is_none())
        .map(path_key)
        .collect();
    paths.sort();
    paths.dedup();
    paths
}

/// How old the catalog must be before a rescan, or `None` with nothing to
/// find. The doubling backoff only throttles misses already seen at the
/// last rescan (a redistributable or tool with no manifest); any new
/// unmatched window, such as a freshly installed game, uses the base
/// interval again.
fn refresh_wait_for(
    unmatched: &[String],
    unmatched_at_last_refresh: &[String],
    backoff: Duration,
) -> Option<Duration> {
    if unmatched.is_empty() {
        return None;
    }
    let new_miss = unmatched
        .iter()
        .any(|path| !unmatched_at_last_refresh.contains(path));
    Some(if new_miss {
        STEAM_CATALOG_REFRESH_INTERVAL
    } else {
        backoff
    })
}

fn maybe_refresh_steam_catalog(
    steam: &mut SteamDetectorState,
    candidates: &[CapturableWindow],
) {
    let (unmatched, age) = {
        let catalog = steam.catalog_mut();
        (unmatched_steam_paths(catalog, candidates), catalog.loaded_at.elapsed())
    };
    let Some(wait) = refresh_wait_for(&unmatched, &steam.unmatched_at_last_refresh, steam.refresh_wait)
    else {
        return;
    };
    if !steam.live || age < wait {
        return;
    }
    let refreshed = crate::game_discovery::SteamLaunchCatalog::scan();
    let changed = {
        let catalog = steam.catalog_mut();
        let changed = refreshed
            .apps
            .iter()
            .any(|app| !catalog.apps.iter().any(|old| old.app_id == app.app_id));
        *catalog = refreshed;
        changed
    };
    steam.refresh_wait = next_refresh_wait(wait, changed);
    steam.unmatched_at_last_refresh = unmatched;
}

fn next_refresh_wait(current: Duration, changed: bool) -> Duration {
    if changed {
        STEAM_CATALOG_REFRESH_INTERVAL
    } else {
        (current * 2).min(STEAM_CATALOG_REFRESH_MAX_BACKOFF)
    }
}
fn find_best_steam_match<'a>(
    catalog: &'a crate::game_discovery::SteamLaunchCatalog,
    candidates: &'a [CapturableWindow],
    preferred_app_id: Option<u32>,
    settings: &GameSettings,
) -> Option<(&'a crate::game_discovery::SteamLaunchApp, &'a CapturableWindow)> {
    // "Never ask again" apps drop out here, so they neither prompt nor hide
    // another Steam game behind them.
    let matched: Vec<_> = candidates
        .iter()
        .filter_map(|window| Some((catalog.find_by_exe_path(window.exe_path.as_deref()?)?, window)))
        .filter(|(app, _)| !settings.steam_app_ignored(app.app_id))
        .collect();
    // The app already being captured wins while it has a window. Otherwise
    // window (z-order) order picks among different games.
    let best_app = matched
        .iter()
        .find(|(app, _)| Some(app.app_id) == preferred_app_id)
        .or_else(|| matched.first())?
        .0;

    // Several windows can share one Steam app (game + splash/launcher);
    // the longest title among windows for this app is the actual gameplay window.
    let best_window = matched
        .iter()
        .filter(|(app, _)| app.app_id == best_app.app_id)
        .max_by_key(|(_, window)| window.title.len())?
        .1;

    Some((best_app, best_window))
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
            ignored_steam_games: Vec::new(),
            plugins: settings_with_all_plugins_disabled().plugins,
            custom_games: Vec::new(),
        }));
        assert!(!has_enabled_games(&GameSettings {
            auto_detect: true,
            pause_when_no_game: false,
            auto_detect_steam_launches: false,
            ignored_steam_games: Vec::new(),
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
            ignored_steam_games: Vec::new(),
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

    /// Steam launch detection defaults off unless the user records games-only;
    /// these tests exercise the fallback itself.
    fn steam_enabled() -> GameSettings {
        GameSettings {
            auto_detect_steam_launches: true,
            ..GameSettings::default()
        }
    }

    fn steam_catalog() -> crate::game_discovery::SteamLaunchCatalog {
        crate::game_discovery::SteamLaunchCatalog {
            apps: vec![crate::game_discovery::SteamLaunchApp::new(
                427520,
                "Friendslop",
                r"C:\Steam\steamapps\common\Friendslop",
            )],
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
            &steam_enabled(),
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
    fn never_ask_again_steam_app_is_skipped_for_the_next_steam_game() {
        let mut settings = steam_enabled();
        settings.ignored_steam_games = vec![crate::settings::IgnoredSteamGame {
            app_id: 427520,
            name: "Friendslop".into(),
        }];
        let mut catalog = steam_catalog();
        catalog.apps.push(crate::game_discovery::SteamLaunchApp::new(
            646570,
            "Slay the Spire",
            r"C:\Steam\steamapps\common\SlayTheSpire",
        ));
        let mut steam = super::SteamDetectorState::fixed(catalog);
        let friendslop = window(
            11,
            "Friendslop",
            "Friendslop.exe",
            Some(r"C:\Steam\steamapps\common\Friendslop\Friendslop.exe"),
        );

        assert!(super::detect_active_game_from_windows_with_steam(
            &settings,
            vec![friendslop.clone()],
            &mut steam,
        )
        .is_none());

        let detected = super::detect_active_game_from_windows_with_steam(
            &settings,
            vec![
                friendslop,
                window(
                    12,
                    "Slay the Spire",
                    "javaw.exe",
                    Some(r"C:\Steam\steamapps\common\SlayTheSpire\jre\bin\javaw.exe"),
                ),
            ],
            &mut steam,
        )
        .expect("the next Steam game is still detected");
        assert_eq!(detected.identity.id(), "steam-646570");
    }

    #[test]
    fn built_in_plugin_still_wins_over_steam_path() {
        let detected = detect_with_catalog(
            &steam_enabled(),
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
            ignored_steam_games: Vec::new(),
            plugins: settings_with_league(false, GameRecordingMode::FullSession).plugins,
            custom_games: Vec::new(),
        };
        let catalog = crate::game_discovery::SteamLaunchCatalog {
            apps: vec![crate::game_discovery::SteamLaunchApp::new(
                1,
                "League of Legends",
                r"C:\Steam\steamapps\common\League of Legends",
            )],
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
    fn disabled_plugin_filters_every_matching_window() {
        let settings = GameSettings {
            plugins: settings_with_league(false, GameRecordingMode::FullSession).plugins,
            ..GameSettings::default()
        };
        let catalog = crate::game_discovery::SteamLaunchCatalog {
            apps: vec![crate::game_discovery::SteamLaunchApp::new(
                1,
                "League of Legends",
                r"C:\Steam\steamapps\common\League of Legends",
            )],
            loaded_at: std::time::Instant::now(),
        };
        let mut steam = super::SteamDetectorState::fixed(catalog);

        let detected = super::detect_active_game_from_windows_with_steam(
            &settings,
            vec![
                window(
                    7,
                    "League of Legends",
                    "League of Legends.exe",
                    Some(r"C:\Steam\steamapps\common\League of Legends\League of Legends.exe"),
                ),
                window(
                    8,
                    "League of Legends (TM) Client",
                    "League of Legends.exe",
                    Some(r"C:\Steam\steamapps\common\League of Legends\League of Legends.exe"),
                ),
            ],
            &mut steam,
        );

        assert!(detected.is_none());
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
                    &steam_enabled(),
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
            ignored_steam_games: Vec::new(),
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
    fn disabled_custom_rule_is_not_revived_by_steam_path() {
        let settings = GameSettings {
            custom_games: vec![CustomGameSettings {
                id: "custom-friendslop-off".into(),
                enabled: false,
                exe_name: "Friendslop.exe".into(),
                process_path: Some(
                    r"C:\Steam\steamapps\common\Friendslop\Friendslop.exe".into(),
                ),
                window_title: "Friendslop".into(),
                recording_mode: GameRecordingMode::ReplaysOnly,
                ..game()
            }],
            ..steam_enabled()
        };
        let windows = vec![window(
            6,
            "Friendslop",
            "Friendslop.exe",
            Some(r"C:\Steam\steamapps\common\Friendslop\Friendslop.exe"),
        )];

        assert!(
            detect_with_catalog(&settings, windows.clone()).is_none(),
            "a disabled custom rule must keep owning its Steam-path window"
        );

        // A path-less rule naming the exact exe is the user saying "don't
        // record this"; recording it through the fallback would ignore that.
        let exe_only = GameSettings {
            custom_games: vec![CustomGameSettings {
                id: "custom-exe-off".into(),
                enabled: false,
                exe_name: "friendslop.EXE".into(),
                process_path: None,
                window_title: String::new(),
                ..game()
            }],
            ..steam_enabled()
        };
        assert!(
            detect_with_catalog(&exe_only, windows.clone()).is_none(),
            "a disabled rule naming the exact exe must keep owning the window"
        );

        // Title substrings are fuzzy and could name an unrelated game.
        let title_only = GameSettings {
            custom_games: vec![CustomGameSettings {
                id: "custom-title-off".into(),
                enabled: false,
                exe_name: String::new(),
                process_path: None,
                window_title: "Friend".into(),
                ..game()
            }],
            ..steam_enabled()
        };
        assert!(
            detect_with_catalog(&title_only, windows.clone()).is_some(),
            "a title-only disabled rule must not block the Steam fallback"
        );

        let unrelated = GameSettings {
            custom_games: vec![CustomGameSettings {
                id: "custom-other-off".into(),
                enabled: false,
                exe_name: "OtherGame.exe".into(),
                process_path: Some(r"C:\Steam\steamapps\common\OtherGame\OtherGame.exe".into()),
                window_title: "Other Game".into(),
                ..game()
            }],
            ..steam_enabled()
        };
        assert!(
            detect_with_catalog(&unrelated, windows).is_some(),
            "an unrelated disabled rule must not block the Steam fallback"
        );
    }

    #[test]
    fn steam_refresh_backoff_doubles_until_a_change_resets_it() {
        let mut wait = next_refresh_wait(STEAM_CATALOG_REFRESH_INTERVAL, false);
        assert_eq!(wait, Duration::from_secs(60));
        wait = next_refresh_wait(wait, false);
        assert_eq!(wait, Duration::from_secs(120));
        wait = next_refresh_wait(wait, false);
        assert_eq!(wait, Duration::from_secs(240));
        wait = next_refresh_wait(wait, false);
        wait = next_refresh_wait(wait, false);
        assert_eq!(wait, STEAM_CATALOG_REFRESH_MAX_BACKOFF);
        assert_eq!(
            next_refresh_wait(wait, true),
            STEAM_CATALOG_REFRESH_INTERVAL
        );
    }

    #[test]
    fn a_catalog_hit_does_not_hide_another_rooted_candidate_miss() {
        let catalog = steam_catalog();
        let candidates = vec![
            window(
                1,
                "Friendslop",
                "Friendslop.exe",
                Some(r"C:\Steam\steamapps\common\Friendslop\Friendslop.exe"),
            ),
            window(
                2,
                "New Game",
                "NewGame.exe",
                Some(r"C:\Steam\steamapps\common\NewGame\NewGame.exe"),
            ),
        ];

        assert_eq!(
            unmatched_steam_paths(&catalog, &candidates),
            [path_key(r"C:\Steam\steamapps\common\NewGame\NewGame.exe")]
        );
    }

    #[test]
    fn a_library_outside_the_scanned_roots_still_counts_as_a_miss() {
        // Added after the first scan (or Steam installed later): its root is
        // not in the catalog yet, which is exactly why it needs a rescan.
        let candidates = vec![window(
            3,
            "New Game",
            "NewGame.exe",
            Some(r"D:\SteamLibrary\steamapps\common\NewGame\NewGame.exe"),
        )];

        assert_eq!(unmatched_steam_paths(&steam_catalog(), &candidates).len(), 1);
    }

    #[test]
    fn a_newly_unmatched_window_skips_the_backoff_for_old_misses() {
        let tool = path_key(r"C:\Steam\steamapps\common\Redist\setup.exe");
        let game = path_key(r"C:\Steam\steamapps\common\NewGame\NewGame.exe");
        let backed_off = STEAM_CATALOG_REFRESH_MAX_BACKOFF;

        let tool_only = std::slice::from_ref(&tool);

        assert_eq!(refresh_wait_for(&[], tool_only, backed_off), None);
        assert_eq!(
            refresh_wait_for(tool_only, tool_only, backed_off),
            Some(backed_off),
            "the same unmatched tool keeps its backoff"
        );
        assert_eq!(
            refresh_wait_for(&[tool.clone(), game], tool_only, backed_off),
            Some(STEAM_CATALOG_REFRESH_INTERVAL),
            "a newly launched game is rescanned on the base interval"
        );
    }

    #[test]
    fn live_steam_catalog_initializes_from_a_real_scan() {
        // Device test: proves the live init path scans this machine's Steam
        // instead of installing the empty stub that broke the refresh gate.
        // CI runners and many dev machines have no Steam install to observe.
        if std::env::var_os("CI").is_some()
            || crate::game_discovery::steam_install_roots()
                .unwrap_or_default()
                .is_empty()
        {
            eprintln!("SKIP: no Steam install on this machine");
            return;
        }
        let mut state = SteamDetectorState::live();
        assert!(state.catalog.is_none(), "construction stays lazy");
        let live_apps = state.catalog_mut().apps.len();
        assert_eq!(
            live_apps,
            crate::game_discovery::SteamLaunchCatalog::scan().apps.len(),
            "live init must populate the catalog via scan(), not an empty stub"
        );
    }

    #[test]
    fn path_under_install_dir_matches_even_when_exe_is_not_the_folder_exe() {
        // Unity/Unreal layout: the running exe is nested and named nothing
        // like the install folder. Lookup is install-dir prefix only.
        let detected = detect_with_catalog(
            &steam_enabled(),
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
            &steam_enabled(),
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

    #[test]
    fn window_order_wins_across_different_steam_games() {
        let catalog = crate::game_discovery::SteamLaunchCatalog {
            apps: vec![
                crate::game_discovery::SteamLaunchApp::new(
                    1,
                    "First",
                    r"C:\Steam\steamapps\common\First",
                ),
                crate::game_discovery::SteamLaunchApp::new(
                    2,
                    "Second",
                    r"C:\Steam\steamapps\common\Second",
                ),
            ],
            loaded_at: std::time::Instant::now(),
        };
        let mut steam = super::SteamDetectorState::fixed(catalog);

        let detected = super::detect_active_game_from_windows_with_steam(
            &steam_enabled(),
            vec![
                window(
                    1,
                    "First",
                    "First.exe",
                    Some(r"C:\Steam\steamapps\common\First\First.exe"),
                ),
                window(
                    2,
                    "Second - A Much Longer Window Title",
                    "Second.exe",
                    Some(r"C:\Steam\steamapps\common\Second\Second.exe"),
                ),
            ],
            &mut steam,
        )
        .expect("a Steam window should match");

        assert_eq!(detected.identity.id(), "steam-1");
        assert_eq!(detected.hwnd, 1);
    }

    #[test]
    fn the_captured_steam_game_stays_detected_across_z_order_changes() {
        let catalog = crate::game_discovery::SteamLaunchCatalog {
            apps: vec![
                crate::game_discovery::SteamLaunchApp::new(
                    1,
                    "First",
                    r"C:\Steam\steamapps\common\First",
                ),
                crate::game_discovery::SteamLaunchApp::new(
                    2,
                    "Second",
                    r"C:\Steam\steamapps\common\Second",
                ),
            ],
            loaded_at: std::time::Instant::now(),
        };
        let mut steam = super::SteamDetectorState::fixed(catalog);
        let first = || {
            window(1, "First", "First.exe", Some(r"C:\Steam\steamapps\common\First\First.exe"))
        };
        let second = || {
            window(2, "Second", "Second.exe", Some(r"C:\Steam\steamapps\common\Second\Second.exe"))
        };
        let mut detect = |windows| {
            super::detect_active_game_from_windows_with_steam(
                &steam_enabled(),
                windows,
                &mut steam,
            )
            .map(|game| game.identity.id().to_owned())
        };

        assert_eq!(detect(vec![first(), second()]).as_deref(), Some("steam-1"));
        // Alt-tab raises Second above First: the capture must not move.
        assert_eq!(detect(vec![second(), first()]).as_deref(), Some("steam-1"));
        // First closes: Second takes over and then keeps the capture.
        assert_eq!(detect(vec![second()]).as_deref(), Some("steam-2"));
        assert_eq!(detect(vec![first(), second()]).as_deref(), Some("steam-2"));
        // No Steam window at all forgets the preference.
        assert_eq!(detect(Vec::new()), None);
        assert_eq!(detect(vec![first(), second()]).as_deref(), Some("steam-1"));
    }

    #[test]
    fn another_game_taking_over_clears_the_steam_preference() {
        let catalog = crate::game_discovery::SteamLaunchCatalog {
            apps: vec![
                crate::game_discovery::SteamLaunchApp::new(
                    1,
                    "First",
                    r"C:\Steam\steamapps\common\First",
                ),
                crate::game_discovery::SteamLaunchApp::new(
                    2,
                    "Second",
                    r"C:\Steam\steamapps\common\Second",
                ),
            ],
            loaded_at: std::time::Instant::now(),
        };
        let mut steam = super::SteamDetectorState::fixed(catalog);
        let settings = GameSettings {
            custom_games: vec![CustomGameSettings {
                id: "custom-other".into(),
                exe_name: "Other.exe".into(),
                process_path: Some(r"C:\Games\Other\Other.exe".into()),
                window_title: "Other".into(),
                ..game()
            }],
            ..steam_enabled()
        };
        let first = || {
            window(1, "First", "First.exe", Some(r"C:\Steam\steamapps\common\First\First.exe"))
        };
        let second = || {
            window(2, "Second", "Second.exe", Some(r"C:\Steam\steamapps\common\Second\Second.exe"))
        };
        let other = || window(3, "Other", "Other.exe", Some(r"C:\Games\Other\Other.exe"));
        let mut detect = |windows| {
            super::detect_active_game_from_windows_with_steam(&settings, windows, &mut steam)
                .map(|game| game.identity.id().to_owned())
        };

        assert_eq!(detect(vec![first(), second()]).as_deref(), Some("steam-1"));
        assert_eq!(detect(vec![other(), second(), first()]).as_deref(), Some("custom-other"));
        // Other closes with Second on top: First must not win on a preference
        // left over from before the custom game took the capture.
        assert_eq!(detect(vec![second(), first()]).as_deref(), Some("steam-2"));
    }
}
