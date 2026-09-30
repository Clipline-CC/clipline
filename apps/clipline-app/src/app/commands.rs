use std::path::PathBuf;

use tauri::{
    AppHandle, Runtime,
};
use tauri_plugin_autostart::ManagerExt;


use crate::game_discovery::DetectedGameCandidate;
use crate::games::DetectedGame;
use crate::game_plugins::GamePluginInfo;
use crate::games::GameWindowInfo;
use crate::service::{self};
use crate::settings::{
    quota_bytes_from_gb, AppSettings,
    CustomGameSettings,
};
use super::*;

#[derive(serde::Serialize)]
pub(crate) struct DisplayInfo {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) x: i32,
    pub(crate) y: i32,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) is_primary: bool,
}

#[derive(serde::Serialize)]
pub(crate) struct AudioDeviceInfo {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) is_default: bool,
}

#[derive(serde::Serialize)]
pub(crate) struct AudioDeviceLists {
    pub(crate) outputs: Vec<AudioDeviceInfo>,
    pub(crate) inputs: Vec<AudioDeviceInfo>,
}

#[tauri::command]
pub(crate) fn save_replay<R: Runtime>(app: AppHandle<R>, state: tauri::State<RuntimeState>) {
    state.request_save_or_show_quota(&app);
}

#[tauri::command]
pub(crate) fn recheck_storage_quota<R: Runtime>(
    app: AppHandle<R>,
    state: tauri::State<RuntimeState>,
    storage_settings: tauri::State<crate::library::StorageSettings>,
    announce: bool,
) -> Result<bool, String> {
    let auto_delete = state.settings().auto_delete_when_over_quota;
    state.recheck_storage_quota(
        app,
        &storage_settings.media_dir(),
        storage_settings.quota_bytes(),
        auto_delete,
        announce,
    )
}

#[tauri::command]
pub(crate) fn restart_as_administrator<R: Runtime>(app: AppHandle<R>) -> Result<bool, String> {
    if crate::windows::current_process_is_elevated()? {
        return Ok(false);
    }
    crate::windows::launch_elevated_after(std::process::id())?;
    quit_app(&app);
    Ok(true)
}

#[tauri::command]
pub(crate) fn get_autostart_status<R: Runtime>(app: AppHandle<R>) -> Result<bool, String> {
    app.autolaunch().is_enabled().map_err(|e| e.to_string())
}

pub(crate) fn set_autostart<R: Runtime>(app: &AppHandle<R>, enabled: bool) -> Result<bool, String> {
    if !autostart_should_mutate_for_current_build() {
        return Ok(enabled);
    }
    let autostart = app.autolaunch();
    if enabled {
        autostart.enable().map_err(|e| e.to_string())?;
    } else {
        autostart.disable().map_err(|e| e.to_string())?;
    }
    autostart.is_enabled().map_err(|e| e.to_string())
}

pub(crate) fn autostart_should_mutate_for_current_build() -> bool {
    autostart_should_mutate_for_build(cfg!(debug_assertions))
}

pub(crate) fn autostart_should_mutate_for_build(debug_build: bool) -> bool {
    !debug_build
}

pub(crate) fn saved_autostart_preference_for_current_build(requested: bool, previous: bool) -> bool {
    saved_autostart_preference_for_build(requested, previous, cfg!(debug_assertions))
}

pub(crate) fn saved_autostart_preference_for_build(
    requested: bool,
    previous: bool,
    debug_build: bool,
) -> bool {
    if debug_build {
        previous
    } else {
        requested
    }
}

/// Whether this build bundles a fixed WebView2 runtime (the "standalone"
/// installer variant). The install mode comes from the Tauri config baked in
/// at compile time, so the answer is a property of the installed binary, not
/// of the machine it runs on.
pub(crate) fn is_standalone_install<R: Runtime>(app: &AppHandle<R>) -> bool {
    matches!(
        app.config().bundle.windows.webview_install_mode,
        tauri::utils::config::WebviewInstallMode::FixedRuntime { .. }
    )
}

#[tauri::command]
pub(crate) fn open_changelog() -> Result<(), String> {
    crate::windows::open_with_shell(
        std::ffi::OsStr::new(crate::updates::CHANGELOG_URL),
        "open changelog",
    )
}

#[tauri::command]
pub(crate) fn get_settings(state: tauri::State<RuntimeState>) -> AppSettings {
    state.settings()
}

#[tauri::command]
pub(crate) fn needs_first_run_setup(state: tauri::State<FirstRunState>) -> bool {
    state.is_pending()
}

pub(crate) async fn choose_folder_dialog(
    title: &'static str,
    current_dir: PathBuf,
) -> Result<Option<PathBuf>, String> {
    // Run the native modal off the main thread so recorder status and other
    // IPC keep flowing while the picker is open.
    tauri::async_runtime::spawn_blocking(move || {
        let mut dialog = rfd::FileDialog::new().set_title(title);
        if current_dir.exists() {
            dialog = dialog.set_directory(current_dir);
        }
        dialog.pick_folder()
    })
    .await
    .map_err(|error| error.to_string())
}

#[tauri::command]
pub(crate) async fn choose_media_folder(
    state: tauri::State<'_, RuntimeState>,
    authorization: tauri::State<'_, NativeMediaFolderAuthorization>,
) -> Result<Option<String>, String> {
    let current_dir = state
        .settings()
        .media_dir_path()
        .ok()
        .filter(|path| path.exists())
        .unwrap_or_else(service::default_clips_dir);

    let selected = choose_folder_dialog("Choose Clipline Media Folder", current_dir).await?;
    let Some(selected) = selected else {
        return Ok(None);
    };
    let selected = crate::settings::normalize_media_dir(&selected.display().to_string())?;
    let selected = selected
        .canonicalize()
        .map_err(|e| format!("resolve selected media folder {selected:?}: {e}"))?;
    authorization.authorize(selected.clone());
    Ok(Some(display_media_folder_path(&selected)))
}

#[tauri::command]
pub(crate) async fn choose_replay_cache_folder(
    state: tauri::State<'_, RuntimeState>,
) -> Result<Option<String>, String> {
    let settings = state.settings();
    let current_dir =
        crate::settings::normalize_replay_cache_dir(&settings.replay_storage.disk_dir)
            .ok()
            .filter(|path| path.exists())
            .or_else(|| settings.media_dir_path().ok())
            .unwrap_or_else(service::default_clips_dir);

    choose_folder_dialog("Choose Clipline Replay Cache Folder", current_dir)
        .await
        .map(|selected| selected.map(|path| path.display().to_string()))
}

#[tauri::command]
pub(crate) fn list_displays() -> Result<Vec<DisplayInfo>, String> {
    clipline_capture::windows::display::enumerate_displays()
        .map_err(|e| e.to_string())
        .map(|displays| {
            displays
                .into_iter()
                .map(|display| DisplayInfo {
                    id: display.id,
                    name: display.name,
                    x: display.x,
                    y: display.y,
                    width: display.width,
                    height: display.height,
                    is_primary: display.is_primary,
                })
                .collect()
        })
}

#[tauri::command]
pub(crate) fn list_audio_devices() -> Result<AudioDeviceLists, String> {
    clipline_capture::windows::wasapi::enumerate_audio_devices()
        .map_err(|e| e.to_string())
        .map(|devices| AudioDeviceLists {
            outputs: devices
                .outputs
                .into_iter()
                .map(|device| AudioDeviceInfo {
                    id: device.id,
                    name: device.name,
                    is_default: device.is_default,
                })
                .collect(),
            inputs: devices
                .inputs
                .into_iter()
                .map(|device| AudioDeviceInfo {
                    id: device.id,
                    name: device.name,
                    is_default: device.is_default,
                })
                .collect(),
        })
}

/// Every encoder this machine can use, for the Settings dropdown. Each
/// option carries its codec key so the frontend can flag codecs the in-app
/// player cannot decode.
///
/// `(async)` so Tauri runs this off the main thread: the first call triggers
/// FFmpeg encoder probing (several test-encode subprocesses, ~5s), which would
/// otherwise freeze the UI since synchronous commands run on the main thread.
#[tauri::command(async)]
pub(crate) fn probe_encoders() -> Vec<service::EncoderOption> {
    service::available_encoder_options()
}

#[tauri::command]
pub(crate) fn list_game_windows() -> Vec<GameWindowInfo> {
    crate::games::list_game_windows()
}

#[tauri::command(async)]
pub(crate) fn detect_installed_games(
    existing_custom_games: Vec<CustomGameSettings>,
) -> Vec<DetectedGameCandidate> {
    crate::game_discovery::detect_installed_games(&existing_custom_games)
}

/// Extract an executable's icon as a PNG `data:` URL for the custom-games UI.
/// Returns `None` when the path has no usable icon.
#[tauri::command]
pub(crate) fn extract_window_icon(process_id: u32) -> Option<String> {
    let path = crate::games::list_game_windows()
        .into_iter()
        .find(|window| window.process_id == process_id)?
        .exe_path?;
    crate::game_icon::extract_exe_icon_data_url(&path)
}

#[tauri::command]
pub(crate) fn list_game_plugins() -> Vec<GamePluginInfo> {
    crate::games::game_plugin_catalog()
}

/// The discovered Steam launch the "Always add" offer was shown for. The
/// frontend never sends an executable path; the rule is rebuilt from the live
/// `DetectedGame`, and the app id keeps a stale offer from adding a different
/// game that now shares the window or process.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DiscoveredSteamTarget {
    pub(crate) app_id: u32,
    pub(crate) process_id: u32,
}

pub(crate) fn discovered_steam_target_matches(
    active: &DetectedGame,
    target: &DiscoveredSteamTarget,
) -> bool {
    matches!(
        active.identity,
        crate::game_identity::GameIdentity::DiscoveredSteam { app_id, .. }
            if app_id == target.app_id
    ) && active.process_id == target.process_id
}

pub(crate) fn custom_game_from_discovered_steam(
    game: &DetectedGame,
) -> Result<CustomGameSettings, String> {
    let crate::game_identity::GameIdentity::DiscoveredSteam { app_id, .. } = &game.identity else {
        return Err("the detected game is not a discovered Steam launch".into());
    };
    let exe_path = game
        .exe_path
        .clone()
        .filter(|path| !path.trim().is_empty())
        .ok_or("the detected Steam window has no executable path")?;
    Ok(CustomGameSettings {
        id: format!("custom-steam-{app_id}"),
        // Sessions recorded before the add carry the discovered identity.
        legacy_ids: vec![format!("steam-{app_id}")],
        name: game.name.clone(),
        enabled: true,
        exe_name: game.exe_name.clone(),
        process_path: Some(exe_path),
        window_title: game.window_title.clone(),
        recording_mode: game.recording_mode,
        icon: None,
    })
}

/// Append `game` unless a rule already targets the same executable, then run
/// the normal custom-game normalization so ids and dedupe match manual adds.
/// Steam ids (`custom-steam-<u32>`, plus a short suffix) stay far below the
/// custom id length limit.
pub(crate) fn insert_discovered_custom_game(
    games: &mut crate::settings::GameSettings,
    mut game: CustomGameSettings,
) -> Option<CustomGameSettings> {
    let path_key = crate::games::path_key(game.process_path.as_deref().unwrap_or_default());
    let covered = games.custom_games.iter().any(|existing| {
        existing
            .process_path
            .as_deref()
            .is_some_and(|path| crate::games::path_key(path) == path_key)
            || (existing.process_path.is_none()
                && !existing.exe_name.trim().is_empty()
                && existing.exe_name.eq_ignore_ascii_case(&game.exe_name))
    });
    if covered {
        return None;
    }
    let base = game.id.clone();
    let mut suffix = 2;
    while games.custom_games.iter().any(|existing| existing.id == game.id) {
        game.id = format!("{base}-{suffix}");
        suffix += 1;
    }
    let id = game.id.clone();
    games.custom_games.push(game);
    games.normalize();
    games.custom_games.iter().find(|existing| existing.id == id).cloned()
}

/// Persist the currently discovered Steam launch as a custom game. Returns
/// the saved rule so the Settings page can merge it into its draft, or `None`
/// when an existing rule already covers the game.
#[tauri::command(async)]
pub(crate) fn add_discovered_steam_game(
    state: tauri::State<RuntimeState>,
    target: DiscoveredSteamTarget,
) -> Result<Option<CustomGameSettings>, String> {
    let mut game = {
        let inner = state.0.lock().map_err(|_| "runtime state lock poisoned")?;
        let active = inner
            .active_game
            .as_ref()
            .ok_or("no game is currently detected")?;
        if !discovered_steam_target_matches(active, &target) {
            return Err("the discovered Steam game is no longer active".into());
        }
        custom_game_from_discovered_steam(active)?
    };
    // Icon extraction reads the executable; keep it outside the runtime lock.
    game.icon = game
        .process_path
        .as_deref()
        .and_then(crate::game_icon::extract_exe_icon_data_url);
    state.add_custom_game_with(game, AppSettings::save)
}

/// The frontend reports which codecs WebView2 can decode (canPlayType) so
/// Automatic selection never records a clip the review player can't show.
/// Takes effect on the next recorder (re)start.
#[tauri::command]
pub(crate) fn report_decode_support(state: tauri::State<RuntimeState>, codecs: Vec<String>) {
    state.set_decodable_codecs(&codecs);
}

pub(crate) fn parse_quota_gb(raw: &str) -> Result<Option<u64>, &'static str> {
    let gb = raw.parse::<f64>().map_err(|_| "expected a number of GiB")?;
    if !gb.is_finite() || gb < 0.0 {
        return Err("quota must be a non-negative finite number");
    }
    if gb == 0.0 {
        return Ok(None);
    }
    quota_bytes_from_gb(gb).map_err(|_| "quota is too large")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quota_parser_converts_gib_to_bytes() {
        assert_eq!(parse_quota_gb("1").unwrap(), Some(1024 * 1024 * 1024));
        assert_eq!(parse_quota_gb("0.5").unwrap(), Some(512 * 1024 * 1024));
    }

    #[test]
    fn quota_parser_zero_disables_quota_lock() {
        assert_eq!(parse_quota_gb("0").unwrap(), None);
    }

    #[test]
    fn quota_parser_rejects_negative_or_non_numeric_values() {
        assert!(parse_quota_gb("-1").is_err());
        assert!(parse_quota_gb("nope").is_err());
    }

    #[test]
    fn debug_build_autostart_policy_skips_registry_mutation() {
        assert!(!autostart_should_mutate_for_build(true));
        assert!(autostart_should_mutate_for_build(false));
    }

    #[test]
    fn debug_build_preserves_saved_autostart_preference() {
        assert!(saved_autostart_preference_for_build(false, true, true));
        assert!(!saved_autostart_preference_for_build(true, false, true));
        assert!(saved_autostart_preference_for_build(true, false, false));
        assert!(!saved_autostart_preference_for_build(false, true, false));
    }

    #[test]
    fn release_build_autostart_policy_honors_user_choice() {
        assert!(saved_autostart_preference_for_build(true, false, false));
        assert!(!saved_autostart_preference_for_build(false, true, false));
    }

    fn target(app_id: u32, process_id: u32) -> DiscoveredSteamTarget {
        DiscoveredSteamTarget { app_id, process_id }
    }

    #[test]
    fn always_add_target_requires_the_same_live_steam_app_and_process() {
        let active = crate::app::discovered_steam_game(42);

        assert!(discovered_steam_target_matches(&active, &target(427520, 42)));
        assert!(!discovered_steam_target_matches(&active, &target(427520, 43)));
        assert!(!discovered_steam_target_matches(&active, &target(1, 42)));
        assert!(!discovered_steam_target_matches(
            &crate::app::detected_game("custom", "Friendslop", 42),
            &target(427520, 42),
        ));
    }

    #[test]
    fn always_add_builds_a_linked_rule_and_dedupes_by_path() {
        let game =
            custom_game_from_discovered_steam(&crate::app::discovered_steam_game(42)).unwrap();
        assert_eq!(game.id, "custom-steam-427520");
        assert_eq!(game.legacy_ids, ["steam-427520"]);
        assert!(game.icon.is_none(), "icon extraction happens after the runtime lock");

        let mut games = crate::settings::GameSettings::default();
        assert_eq!(insert_discovered_custom_game(&mut games, game.clone()), Some(game.clone()));
        assert_eq!(insert_discovered_custom_game(&mut games, game), None);
        assert_eq!(games.custom_games.len(), 1);
    }

    #[test]
    fn always_add_suffixes_an_id_taken_by_another_game() {
        let game =
            custom_game_from_discovered_steam(&crate::app::discovered_steam_game(42)).unwrap();
        let mut games = crate::settings::GameSettings::default();
        games.custom_games.push(CustomGameSettings {
            process_path: Some(r"C:\Games\Elsewhere\Other.exe".into()),
            exe_name: "Other.exe".into(),
            name: "Other".into(),
            ..game.clone()
        });

        let added = insert_discovered_custom_game(&mut games, game).unwrap();

        assert_eq!(added.id, "custom-steam-427520-2");
        assert_eq!(games.custom_games.len(), 2);
    }
}
