use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tauri::image::Image;
use tauri::menu::{Menu, MenuItem};
use tauri::path::BaseDirectory;
use tauri::tray::{TrayIconBuilder, TrayIconEvent};
use tauri::{
    AppHandle, Emitter, Manager, Runtime, WindowEvent,
};
use tauri_plugin_autostart::ManagerExt;
use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};

use clipline_capture::diagnostics::install_diagnostic_handler;

use crate::games::DetectedGame;
use crate::service::{self, Cmd};
use crate::settings::{
    parse_hotkey, quota_bytes_from_gb, AppSettings, CaptureMode,
};
use super::*;

pub(crate) const MAIN_WINDOW_LABEL: &str = "main";

pub(crate) const WEBVIEW_READY_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) const GAME_DETECTOR_INTERVAL: Duration = Duration::from_millis(500);

pub(crate) const WINDOW_LIFECYCLE_EVENT: &str = "window-lifecycle";

// Frontend readiness is tracked per window generation via
// FrontendReadinessState. The repair dialog remains process-global.
pub(crate) static WEBVIEW_REPAIR_NOTICE_SHOWN: AtomicBool = AtomicBool::new(false);

pub(crate) struct FirstRunState(pub(crate) AtomicBool);

impl FirstRunState {
    pub(crate) fn new(first_run: bool) -> Self {
        Self(AtomicBool::new(first_run))
    }

    pub(crate) fn is_pending(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    pub(crate) fn complete(&self) {
        self.0.store(false, Ordering::Release);
    }
}

pub(crate) fn configure_bundled_ffmpeg<R: Runtime>(app: &tauri::App<R>) {
    match app
        .path()
        .resolve("ffmpeg/ffmpeg.exe", BaseDirectory::Resource)
    {
        Ok(path) if path.exists() => {
            clipline_capture::ffmpeg::set_bundled_ffmpeg(path.clone());
            log_diagnostic(format!("bundled ffmpeg resource={path:?}"));
        }
        Ok(path) => {
            log_diagnostic(format!("bundled ffmpeg resource missing at {path:?}"));
        }
        Err(e) => {
            log_diagnostic(format!("resolve bundled ffmpeg resource failed: {e}"));
        }
    }
}

pub(crate) struct TrayItems<R: Runtime> {
    pub(crate) save_item: MenuItem<R>,
}

impl<R: Runtime> TrayItems<R> {
    pub(crate) fn set_hotkey_label(&self, label: &str) -> Result<(), String> {
        self.save_item
            .set_text(save_menu_text(label))
            .map_err(|e| e.to_string())
    }
}

pub fn run() {
    let _diagnostics_guard = diagnostics::init().ok();
    if let Err(error) = install_diagnostic_handler(|event| log_diagnostic(event.to_string())) {
        log_diagnostic(format!("capture diagnostic setup: {error}"));
    }
    let startup_load = AppSettings::load_for_startup();
    let first_run = startup_load.first_run;
    let mut settings = startup_load.settings;
    let mut startup_warnings = startup_load.warnings;
    for warning in &startup_warnings {
        log_diagnostic(format!("settings recovery: {warning}"));
        tracing::warn!(event = "settings_recovery_warning", message = %warning);
    }
    let args: Vec<String> = std::env::args().collect();
    log_diagnostic(format!(
        "run start version={} args={args:?} log_path={:?}",
        env!("CARGO_PKG_VERSION"),
        diagnostic_log_path()
    ));
    log_diagnostic(webview2_runtime_diagnostic());
    let mut lol_url = None::<String>;
    if let Some(i) = args.iter().position(|a| a == "--window") {
        if let Some(title) = args.get(i + 1) {
            settings.capture_mode = CaptureMode::WindowTitle;
            settings.window_title = title.clone();
        }
    }
    if let Some(i) = args.iter().position(|a| a == "--lol-url") {
        lol_url = args.get(i + 1).cloned();
    }
    if let Some(i) = args.iter().position(|a| a == "--disk-quota-gb") {
        match args
            .get(i + 1)
            .ok_or("missing --disk-quota-gb value")
            .and_then(|v| parse_quota_gb(v).map(|_| v))
        {
            Ok(v) => {
                if let Ok(gb) = v.parse::<f64>() {
                    settings.disk_quota_gb = gb;
                }
            }
            Err(e) => tracing::warn!(event = "command_line_quota_invalid", error = %e),
        }
    }
    if let Err(e) = settings.validate() {
        let warning = format!(
            "Clipline started with safe defaults because command-line settings were invalid: {e}"
        );
        log_diagnostic(&warning);
        tracing::warn!(event = "command_line_settings_invalid", message = %warning);
        startup_warnings.push(warning);
        settings = AppSettings::default();
    }

    let quota_bytes = quota_bytes_from_gb(settings.disk_quota_gb)
        .unwrap_or(Some(service::DEFAULT_DISK_QUOTA_BYTES));
    let media_dir = settings
        .media_dir_path()
        .unwrap_or_else(|_| service::default_clips_dir());
    let media_dir_for_setup = media_dir.clone();
    let startup_global_hotkeys =
        global_hotkeys(&settings).unwrap_or_else(|_| vec![parse_hotkey("F6").unwrap()]);

    tauri::Builder::default()
        .manage(RuntimeState::new(settings.clone(), lol_url))
        .manage(FirstRunState::new(first_run))
        .manage(StartupWarnings::new(startup_warnings))
        .manage(WindowLifecycleState::default())
        .manage(MainWindowOpenQueue::default())
        .manage(FrontendReadinessState::default())
        .manage(crate::ffmpeg_install::FfmpegInstallController::default())
        .manage(MicTestState::default())
        .manage(support::SupportState::default())
        .manage(crate::memory::MemorySampler::default())
        .manage(NativeMediaFolderAuthorization::default())
        .manage(crate::library::StorageSettings::new(quota_bytes, media_dir))
        .manage(crate::library::ClipboardExportState::default())
        .plugin(tauri_plugin_single_instance::init(|app, args, cwd| {
            let launched_by_autostart = args.iter().any(|arg| arg == "--autostart");
            log_diagnostic(format!(
                "single-instance secondary launch launched_by_autostart={launched_by_autostart} cwd={cwd:?} args={args:?}"
            ));
            if !launched_by_autostart {
                if let Err(e) = open_main_window(app) {
                    log_diagnostic(format!("single-instance open existing failed: {e}"));
                    tracing::error!(event = "single_instance_window_open_failed", error = %e);
                }
            }
        }))
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--autostart"]),
        ))
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(move |_app, shortcut, event| {
                    if event.state() == ShortcutState::Pressed {
                        let state = _app.state::<RuntimeState>();
                        if state.active_shortcut_matches(shortcut) {
                            state.request_save_or_show_quota(_app);
                        }
                    }
                })
                .build(),
        )
        .invoke_handler(tauri::generate_handler![
            commands::save_replay,
            commands::recheck_storage_quota,
            commands::restart_as_administrator,
            window::set_recording,
            window::set_session_recording,
            commands::get_settings,
            commands::needs_first_run_setup,
            window::minimize_main_window,
            commands::choose_media_folder,
            commands::choose_replay_cache_folder,
            commands::list_displays,
            commands::list_audio_devices,
            commands::probe_encoders,
            commands::report_decode_support,
            commands::list_game_plugins,
            commands::list_game_windows,
            commands::detect_installed_games,
            commands::extract_window_icon,
            webview::memory_status,
            webview::frontend_ready,
            crate::ffmpeg_install::ffmpeg_runtime_status,
            crate::ffmpeg_install::ensure_ffmpeg_runtime,
            crate::ffmpeg_install::cancel_ffmpeg_runtime_install,
            media::start_microphone_test,
            media::stop_microphone_test,
            commands::get_autostart_status,
            updates::check_for_updates,
            updates::install_update,
            commands::open_changelog,
            settings_txn::save_settings,
            hotkeys::set_hotkey_capture_active,
            support::bundle::prepare_bug_report,
            support::bundle::submit_bug_report,
            support::cancel_bug_report,
            support::discard_bug_report,
            support::save_prepared_bug_report,
            support::open_diagnostics_folder,
            support::diagnostics_location,
            support::support_capabilities,
            support::log_frontend_event,
            crate::cloud::cloud_status,
            crate::cloud::cloud_connect,
            crate::cloud::cloud_disconnect,
            crate::cloud::upload_clip_to_cloud,
            crate::cloud::sync_cloud_clip_status,
            crate::cloud::list_cloud_clips,
            crate::cloud::cloud_clip_thumbnail,
            crate::cloud::cache_cloud_clip_media,
            crate::cloud::cloud_user_profile,
            crate::cloud::cloud_user_avatar,
            crate::cloud::open_cloud_user_profile,
            crate::cloud::open_cloud_clip,
            crate::osu_api::osu_api_status,
            crate::osu_api::save_osu_api_settings,
            crate::osu_api::test_osu_api_connection,
            crate::osu_api::open_osu_api_setup_guide,
            crate::library::list_clips,
            crate::library::clip_poster,
            crate::library::delete_clip,
            crate::library::delete_clips,
            crate::library::rename_clip,
            crate::library::rename_clip_file,
            crate::library::set_clip_favorite,
            crate::library::export_clip,
            crate::library::groups::export_group,
            crate::library::groups::reorder_group,
            crate::library::groups::remove_from_group,
            crate::library::prepare_clip_audio_sidecars,
            crate::library::reveal_clip,
            crate::library::copy_clip_to_clipboard,
            crate::library::copy_text_to_clipboard,
            crate::library::open_media_folder,
            crate::library::storage_status
        ])
        .setup(move |app| {
            configure_bundled_ffmpeg(app);
            let osu_app = app.handle().clone();
            let osu_media_root = media_dir_for_setup.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = crate::osu_api::retry_pending_enrichment(&osu_app, osu_media_root).await
                {
                    tracing::warn!(event = "startup_osu_enrichment_retry_failed", error = %e);
                }
            });
            for hotkey in &startup_global_hotkeys {
                if let Err(e) = app.global_shortcut().register(*hotkey) {
                    let message =
                        format!("global save hotkey unavailable; continuing without it: {e}");
                    tracing::warn!(event = "global_hotkey_registration_failed", message = %message);
                    let _ = app.handle().emit("error", message);
                }
            }
            let startup_save_hotkeys = settings.hotkeys();
            let startup_recording_hotkeys = settings.recording_hotkeys();
            let startup_bookmark_hotkeys = settings.bookmark_hotkeys();
            if let Err(e) = crate::hotkeys::install_hotkey_hook(
                crate::hotkeys::HookHotkeys {
                    save: &startup_save_hotkeys,
                    recording: &startup_recording_hotkeys,
                    bookmark: &startup_bookmark_hotkeys,
                },
                {
                let app = app.handle().clone();
                move |trigger| match trigger.action {
                    crate::hotkeys::HookAction::SaveReplay => {
                        app.state::<RuntimeState>().request_save_or_show_quota(&app);
                    }
                    crate::hotkeys::HookAction::ToggleRecording => {
                        if let Err(error) = app
                            .state::<RuntimeState>()
                            .toggle_session_recording_from_hotkey(app.clone())
                        {
                            let _ = app.emit("error", error);
                        }
                    }
                    crate::hotkeys::HookAction::Bookmark => {
                        app.state::<RuntimeState>()
                            .request_bookmark(&app, trigger.pressed_at);
                    }
                }
            },
            ) {
                let message = format!("low-level hotkey unavailable: {e}");
                tracing::warn!(event = "hotkey_hook_install_failed", message = %message);
                let _ = app.handle().emit("error", message);
            }
            if let Err(e) = crate::library::prune_audio_preview_cache_on_startup() {
                tracing::warn!(event = "audio_preview_startup_prune_failed", error = %e);
            }
            spawn_update_poller(app.handle().clone());
            if let Some(local) = std::env::var_os("LOCALAPPDATA").map(std::path::PathBuf::from) {
                let staging = crate::ffmpeg_install::staging_root(&local);
                if let Err(e) = crate::ffmpeg_install::sweep_abandoned_staging(&staging) {
                    tracing::warn!(event = "ffmpeg_staging_startup_sweep_failed", error = %e);
                }
            }

            // Keep release builds in sync with the user's setting. Debug builds
            // share settings and registry state with installed builds, so cargo
            // runs must not disable or replace the installed autostart entry.
            if autostart_should_mutate_for_current_build() {
                let autostart = app.autolaunch();
                let _ = if settings.open_on_startup {
                    autostart.enable()
                } else {
                    autostart.disable()
                };
            }

            // When launched by the autostart registry entry, start in the tray
            // instead of flashing the main window.
            let launched_by_autostart = std::env::args().any(|arg| arg == "--autostart");
            log_diagnostic(format!(
                "setup start launched_by_autostart={launched_by_autostart} webviews={}",
                webview_labels(app.handle())
            ));

            let save_item = MenuItem::with_id(
                app,
                "save",
                save_menu_text(&save_hotkey_label(&settings)),
                true,
                None::<&str>,
            )?;
            let open_item = MenuItem::with_id(app, "open", "Open Clipline", true, None::<&str>)?;
            let diagnostics_item = MenuItem::with_id(
                app,
                "diagnostics",
                "Open Diagnostics Folder",
                true,
                None::<&str>,
            )?;
            let quit_item = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu =
                Menu::with_items(app, &[&open_item, &save_item, &diagnostics_item, &quit_item])?;
            app.manage(TrayItems {
                save_item: save_item.clone(),
            });
            TrayIconBuilder::with_id("clipline")
                .icon(tray_icon())
                .tooltip("Clipline — replay buffer")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(move |app, event| match event.id().as_ref() {
                    "open" => {
                        log_diagnostic("tray menu event: open");
                        if let Err(e) = open_main_window(app) {
                            log_diagnostic(format!("tray menu open failed: {e}"));
                            tracing::error!(event = "tray_window_open_failed", error = %e);
                        }
                    }
                    "save" => {
                        log_diagnostic("tray menu event: save");
                        app.state::<RuntimeState>().request_save_or_show_quota(app);
                    }
                    "diagnostics" => {
                        log_diagnostic("tray menu event: diagnostics");
                        if let Err(error) = support::open_diagnostics_folder() {
                            tracing::error!(
                                event = "open_diagnostics_folder_failed",
                                error = %error
                            );
                        }
                    }
                    "quit" => {
                        log_diagnostic("tray menu event: quit");
                        quit_app(app);
                    }
                    other => {
                        log_diagnostic(format!("tray menu event: unknown id={other}"));
                    }
                })
                .on_tray_icon_event(|tray, event| {
                    if !matches!(event, TrayIconEvent::Move { .. }) {
                        log_diagnostic(format!("tray icon event: {event:?}"));
                    }
                    if should_open_on_tray_event(&event) {
                        log_diagnostic("tray icon event requests open");
                        if let Err(e) = open_main_window(tray.app_handle()) {
                            log_diagnostic(format!("tray icon open failed: {e}"));
                            tracing::error!(event = "tray_icon_window_open_failed", error = %e);
                        }
                    }
                })
                .build(app)?;
            log_diagnostic(format!("tray build complete webviews={}", webview_labels(app.handle())));

            if !first_run {
                if let Err(e) = app
                    .state::<RuntimeState>()
                    .start_recording(app.handle().clone())
                {
                    let message = format!("recorder startup failed: {e}");
                    tracing::error!(event = "recorder_startup_failed", message = %message);
                    let _ = app.handle().emit("error", message);
                }
            }
            spawn_game_detector(app.handle().clone());

            // `"create": false` keeps cold --autostart WebView-free. Normal
            // launches and tray Open build through open_main_window.
            if !launched_by_autostart {
                log_diagnostic("normal launch opening main window");
                if let Err(e) = open_main_window(app.handle()) {
                    log_diagnostic(format!("normal launch open failed: {e}"));
                    tracing::error!(event = "startup_window_show_failed", error = %e);
                }
            } else {
                log_diagnostic("autostart launch leaving Destroyed shell without webview");
            }

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("build tauri app")
        .run(move |app, event| match event {
            tauri::RunEvent::WindowEvent {
                label,
                event: WindowEvent::CloseRequested { api, .. },
                ..
            } if is_app_window_label(&label) => {
                log_diagnostic(format!("window event: app close requested label={label}"));
                api.prevent_close();
                match close_request_action(&app.state::<RuntimeState>().settings()) {
                    CloseRequestAction::Tray => {
                        log_diagnostic("close request action: tray");
                        if let Err(e) = send_main_window_to_tray(app) {
                            log_diagnostic(format!("close to tray failed: {e}"));
                            tracing::error!(event = "close_to_tray_failed", error = %e);
                        }
                    }
                    CloseRequestAction::Quit => {
                        log_diagnostic("close request action: quit");
                        quit_app(app);
                    }
                }
            }
            tauri::RunEvent::WindowEvent {
                label,
                event: WindowEvent::Destroyed,
                ..
            } if is_app_window_label(&label) => {
                log_diagnostic(format!("window event: app Destroyed label={label}"));
                if let Err(e) = complete_main_window_destroyed(app) {
                    log_diagnostic(format!("complete Destroyed failed: {e}"));
                    tracing::error!(event = "main_window_destroyed_handler_failed", error = %e);
                }
            }
            tauri::RunEvent::WindowEvent {
                label,
                event,
                ..
            } if is_app_window_label(&label) && should_reconcile_native_window_event(&event) => {
                if should_log_window_event(&event) {
                    log_diagnostic(format!("window event: label={label} event={event:?}"));
                }
                if let Some(window) = app.get_webview_window(&label) {
                    if let Err(error) = reconcile_native_window(app, &window) {
                        log_diagnostic(format!(
                            "native window reconciliation failed label={label}: {error}"
                        ));
                    }
                }
            }
            tauri::RunEvent::WindowEvent { label, event, .. } => {
                if should_log_window_event(&event) {
                    log_diagnostic(format!("window event: label={label} event={event:?}"));
                }
            }
            tauri::RunEvent::ExitRequested {
                code: None, api, ..
            } => {
                log_diagnostic("exit requested without code; preventing exit");
                api.prevent_exit();
            }
            tauri::RunEvent::Exit => {
                log_diagnostic("run event: exit");
                app.state::<MicTestState>().stop();
                app.state::<crate::library::ClipboardExportState>().cancel();
                app.state::<RuntimeState>()
                    .send(Cmd::Stop { announce: false });
            }
            _ => {}
        });
}

pub(crate) fn spawn_game_detector<R: Runtime>(app: AppHandle<R>) {
    std::thread::Builder::new()
        .name("clipline-game-detector".into())
        .spawn(move || {
            let mut last_error = None::<String>;
            loop {
                std::thread::sleep(GAME_DETECTOR_INTERVAL);
                let settings = app.state::<RuntimeState>().settings();
                let detected = crate::games::detect_active_game(&settings.games);
                let league_lookup = league_gate_applies(&settings, detected.as_ref()).then(|| {
                    Box::new(|game: &DetectedGame| spawn_gate_lookup(game)) as LeagueGateLookup
                });
                match app
                    .state::<RuntimeState>()
                    .set_detected_game(app.clone(), detected, league_lookup)
                {
                    Ok(()) => last_error = None,
                    Err(e) if last_error.as_deref() != Some(e.as_str()) => {
                        last_error = Some(e.clone());
                        let _ = app.emit("error", format!("game detection: {e}"));
                    }
                    Err(_) => {}
                }
                if let Err(e) = app.state::<RuntimeState>().tick_league_gate(app.clone()) {
                    if last_error.as_deref() != Some(e.as_str()) {
                        last_error = Some(e.clone());
                        let _ = app.emit("error", format!("game detection: {e}"));
                    }
                }
            }
        })
        .expect("spawn game detector thread");
}

pub(crate) fn save_menu_text(label: &str) -> String {
    format!("Save Replay ({label})")
}

/// Procedural 32x32 tray icon: a recording dot on a dark rounded square —
/// no asset files, no bundler.
pub(crate) fn tray_icon() -> Image<'static> {
    const N: usize = 32;
    let mut rgba = vec![0u8; N * N * 4];
    for y in 0..N {
        for x in 0..N {
            let i = (y * N + x) * 4;
            let (dx, dy) = (x as f32 - 15.5, y as f32 - 15.5);
            let r = (dx * dx + dy * dy).sqrt();
            let (px, a) = if r < 7.0 {
                ([229u8, 72, 77], 255) // recording red
            } else if r < 15.0 {
                ([24u8, 26, 32], 255) // dark disc
            } else {
                ([0u8, 0, 0], 0)
            };
            rgba[i..i + 3].copy_from_slice(&px);
            rgba[i + 3] = a;
        }
    }
    Image::new_owned(rgba, N as u32, N as u32)
}

#[cfg(test)]
mod tests {
    

    #[test]
    fn native_shell_starts_recorder_after_single_instance_accepts_process() {
        let app = include_str!("setup.rs");
        let run_start = app.find("pub fn run()").expect("run function should exist");
        let run_body = &app[run_start..];
        let run_end = run_body
            .find("\npub(crate) fn spawn_game_detector")
            .expect("run function should be followed by spawn_game_detector");
        let run_body = &run_body[..run_end];
        let single_instance = run_body
            .find("tauri_plugin_single_instance::init")
            .expect("single-instance plugin should be installed");
        let setup = run_body
            .find(".setup(move |app|")
            .expect("app setup should be registered");
        let recorder_start = run_body
            .find("start_recording(app.handle().clone())")
            .expect("setup should start the recorder after plugins are installed");
        let first_run_gate = run_body
            .find("if !first_run")
            .expect("first-run setup must gate initial recorder startup");

        assert!(
            single_instance < setup,
            "single-instance plugin must be installed before setup runs"
        );
        assert!(
            setup < first_run_gate && first_run_gate < recorder_start,
            "initial recorder startup must happen from setup after the first-run gate"
        );
        assert!(
            !run_body[..single_instance].contains("service::spawn("),
            "run() must not spawn the recorder before single-instance can reject a duplicate launch"
        );
    }
}
