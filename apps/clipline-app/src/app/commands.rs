use std::path::PathBuf;

use tauri::{
    AppHandle, Runtime,
};
use tauri_plugin_autostart::ManagerExt;


use crate::game_discovery::DetectedGameCandidate;
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
}
