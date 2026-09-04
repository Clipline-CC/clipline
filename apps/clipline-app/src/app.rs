//! Tauri shell: tray, F6 global hotkey, status webview — all thin
//! wiring around the recorder service thread.
#[path = "app/diagnostics.rs"]
mod diagnostics;
#[path = "app/support.rs"]
mod support;
#[path = "app/lifecycle.rs"]
mod lifecycle;
#[path = "app/shell.rs"]
mod shell;
#[path = "app/window.rs"]
mod window;
#[path = "app/runtime_types.rs"]
mod runtime_types;
#[path = "app/league_gate.rs"]
mod league_gate;
#[path = "app/runtime.rs"]
mod runtime;
#[path = "app/runtime_restart.rs"]
mod runtime_restart;
#[path = "app/runtime_control.rs"]
mod runtime_control;
#[path = "app/commands.rs"]
mod commands;
#[path = "app/settings_txn.rs"]
mod settings_txn;
#[path = "app/hotkeys.rs"]
mod hotkeys;
#[path = "app/updates.rs"]
mod updates;
#[path = "app/media.rs"]
mod media;
#[path = "app/events.rs"]
mod events;
#[path = "app/webview.rs"]
mod webview;
#[path = "app/setup.rs"]
mod setup;
use diagnostics::{diagnostic_log_path, log_diagnostic};
pub(crate) use lifecycle::{FrontendReadinessCheckpoint, FrontendReadinessState, FrontendReadyResponse, MainWindowOpenQueue, WindowLifecycleMode, WindowLifecycleSnapshot, WindowLifecycleState, ensure_foreground_microphone_test, hide_main_window, open_main_window, watchdog_should_fire};
pub(crate) use shell::{MainWindowOpenAction, begin_close_to_tray_destroy, log_window_state, main_window_shell_state, observe_main_window_destroyed, persist_main_window_shell_pending, prepare_frontend_readiness_for_destroy, request_main_window_open, should_open_on_tray_event};
pub(crate) use window::{CloseRequestAction, close_request_action, complete_main_window_destroyed, publish_window_lifecycle, quit_app, reconcile_native_window, request_webview_memory_target, send_main_window_to_tray};
pub(crate) use runtime_types::{CommittedRuntimeRestart, LEAGUE_GATE_SKIP_NOTICE, LeagueGateLookup, LeagueGateResolution, LeagueGateVerdict, PreparedRuntimeRestart, PreparedServiceRestart, RecorderDiagnosticStatus, StorageDiagnosticStatus, active_game_still_configured, emit_waiting_for_game, filter_osu_title_events, game_recording_mode_changed, preserve_backend_owned_settings_fields, record_osu_title_event, recorder_should_run, stopped_status, waiting_for_game_status};
#[cfg(test)]
pub(crate) use runtime_types::detected_built_in_game;
#[cfg(test)]
pub(crate) use runtime_types::detected_game;
pub(crate) use league_gate::{automatic_start_allowed, league_gate_allows, league_gate_applies, spawn_gate_lookup};
#[cfg(test)]
pub(crate) use league_gate::same_game_window;
pub(crate) use runtime::{CLOUD_SETTINGS_SAVE_LOCK, RuntimeInner, RuntimeState};
pub(crate) use commands::{AudioDeviceLists, autostart_should_mutate_for_current_build, is_standalone_install, list_audio_devices, list_displays, parse_quota_gb, probe_encoders, saved_autostart_preference_for_current_build, set_autostart};
#[cfg(test)]
pub(crate) use settings_txn::run_before_releasing_settings_save_lock;
pub(crate) use hotkeys::{effective_global_hotkeys, global_hotkeys, parse_global_hotkey, resume_hotkeys_after_ui_gone, save_hotkey_label, sync_global_hotkeys};
pub(crate) use updates::spawn_update_poller;
pub(crate) use media::{MicTestState, NativeMediaFolderAuthorization, display_media_folder_path};
pub(crate) use events::{GameDetectionEvent, pump_events, should_log_window_event, should_reconcile_native_window_event};
pub(crate) use webview::{StartupWarnings, arm_frontend_ready_watchdog, is_app_window_label, probe_webview_after_reveal, result_debug, webview2_runtime_diagnostic, webview_labels};
pub(crate) use setup::{FirstRunState, MAIN_WINDOW_LABEL, TrayItems, WEBVIEW_READY_TIMEOUT, WEBVIEW_REPAIR_NOTICE_SHOWN, WINDOW_LIFECYCLE_EVENT};
pub use setup::run;
