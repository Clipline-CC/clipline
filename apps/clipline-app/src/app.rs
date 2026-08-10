//! Tauri shell: tray, F6 global hotkey, status webview — all thin
//! wiring around the recorder service thread.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tauri::image::Image;
use tauri::menu::{Menu, MenuItem};
use tauri::path::BaseDirectory;
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{
    AppHandle, Emitter, Manager, Runtime, WebviewWindow, WebviewWindowBuilder, WindowEvent,
};
use tauri_plugin_autostart::ManagerExt;
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};
use tauri_plugin_updater::UpdaterExt;

use clipline_capture::diagnostics::install_diagnostic_handler;

use crate::game_discovery::DetectedGameCandidate;
use crate::game_plugins::GamePluginInfo;
use crate::games::{DetectedGame, GameWindowInfo};
use crate::osu_enrichment::OsuTitleEvent;
use crate::service::{self, Cmd, Event, ServiceOptions};
use crate::settings::{
    is_global_shortcut_hotkey, parse_hotkey, quota_bytes_from_gb, AppSettings, CaptureMode,
    CustomGameSettings, GameRecordingMode,
};
use crate::updates::UpdateChannel;
use crate::util::unix_now_i64;

#[path = "app/diagnostics.rs"]
mod diagnostics;
#[path = "app/support.rs"]
mod support;
use diagnostics::{diagnostic_log_path, log_diagnostic};

const MAIN_WINDOW_LABEL: &str = "main";
const WEBVIEW_READY_TIMEOUT: Duration = Duration::from_secs(5);
const GAME_DETECTOR_INTERVAL: Duration = Duration::from_millis(500);
const WINDOW_LIFECYCLE_EVENT: &str = "window-lifecycle";
// Frontend readiness is tracked per window generation via
// FrontendReadinessState. The repair dialog remains process-global.
static WEBVIEW_REPAIR_NOTICE_SHOWN: AtomicBool = AtomicBool::new(false);

struct FirstRunState(AtomicBool);

impl FirstRunState {
    fn new(first_run: bool) -> Self {
        Self(AtomicBool::new(first_run))
    }

    fn is_pending(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    fn complete(&self) {
        self.0.store(false, Ordering::Release);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum WindowLifecycleMode {
    Foreground,
    /// Legacy soft-hide tray mode. Close-to-tray now uses Destroying/Destroyed;
    /// kept for reconcile coverage and any residual hide paths.
    #[allow(dead_code)]
    Tray,
    Taskbar,
    /// Close-to-tray has requested destroy; the label may still be registered.
    Destroying,
    /// No live app UI. Opens may build a fresh main window.
    Destroyed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
struct WindowLifecycleSnapshot {
    revision: u64,
    mode: WindowLifecycleMode,
    backgrounded: bool,
}

impl WindowLifecycleSnapshot {
    fn new(revision: u64, mode: WindowLifecycleMode) -> Self {
        Self {
            revision,
            mode,
            // Destroying/Destroyed are backgrounded: the UI is gone or going away,
            // so async frontend work must not recreate gallery/media.
            backgrounded: mode != WindowLifecycleMode::Foreground,
        }
    }
}

struct WindowLifecycleState(Mutex<WindowLifecycleSnapshot>);

impl Default for WindowLifecycleState {
    fn default() -> Self {
        // With `"create": false`, cold start (including --autostart) has no
        // WebView until open_main_window builds one. Normal launches move to
        // Foreground after reveal; autostart stays Destroyed.
        Self(Mutex::new(WindowLifecycleSnapshot::new(
            0,
            WindowLifecycleMode::Destroyed,
        )))
    }
}

impl WindowLifecycleState {
    fn snapshot(&self) -> WindowLifecycleSnapshot {
        match self.0.lock() {
            Ok(snapshot) => *snapshot,
            Err(poisoned) => *poisoned.into_inner(),
        }
    }

    fn transition(&self, mode: WindowLifecycleMode) -> WindowLifecycleSnapshot {
        let mut snapshot = match self.0.lock() {
            Ok(snapshot) => snapshot,
            Err(poisoned) => poisoned.into_inner(),
        };
        if snapshot.mode != mode {
            let revision = snapshot.revision.saturating_add(1);
            *snapshot = WindowLifecycleSnapshot::new(revision, mode);
        }
        *snapshot
    }
}

/// Remembers an Open requested while the main window is mid-destroy.
struct MainWindowOpenQueue(AtomicBool);

impl Default for MainWindowOpenQueue {
    fn default() -> Self {
        Self(AtomicBool::new(false))
    }
}

impl MainWindowOpenQueue {
    fn pending(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    fn set_pending(&self, pending: bool) {
        self.0.store(pending, Ordering::Release);
    }
}

/// Per-window UI readiness. Generation 0 means no live main window.
struct FrontendReadinessState {
    /// Monotonic allocator; never resets across destroy/recreate.
    next_generation: AtomicU64,
    /// Currently live UI generation; 0 means no live main window.
    generation: AtomicU64,
    ready_generation: AtomicU64,
    watchdog_armed_generation: AtomicU64,
    destroy_started: Mutex<Option<Instant>>,
}

#[derive(Clone, Copy)]
struct FrontendReadinessCheckpoint {
    generation: u64,
    ready_generation: u64,
}

impl Default for FrontendReadinessState {
    fn default() -> Self {
        Self {
            next_generation: AtomicU64::new(0),
            generation: AtomicU64::new(0),
            ready_generation: AtomicU64::new(0),
            watchdog_armed_generation: AtomicU64::new(0),
            destroy_started: Mutex::new(None),
        }
    }
}

impl FrontendReadinessState {
    fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    fn ready_generation(&self) -> u64 {
        self.ready_generation.load(Ordering::Acquire)
    }

    fn begin_generation(&self) -> u64 {
        let next = self.next_generation.fetch_add(1, Ordering::AcqRel) + 1;
        self.generation.store(next, Ordering::Release);
        if let Ok(mut started) = self.destroy_started.lock() {
            if let Some(started_at) = started.take() {
                let elapsed_ms = started_at.elapsed().as_millis();
                log_diagnostic(format!(
                    "main window recreate generation={next} destroy_to_build_ms={elapsed_ms}"
                ));
            }
        }
        log_diagnostic(format!("main window generation begun generation={next}"));
        next
    }

    fn clear_for_destroy(&self) -> FrontendReadinessCheckpoint {
        let previous = self.generation.swap(0, Ordering::AcqRel);
        let checkpoint = FrontendReadinessCheckpoint {
            generation: previous,
            ready_generation: self.ready_generation(),
        };
        // next_generation stays monotonic so recreate never reuses a ready/watchdog id.
        if let Ok(mut started) = self.destroy_started.lock() {
            *started = Some(Instant::now());
        }
        log_diagnostic(format!(
            "main window generation cleared for destroy previous={previous}"
        ));
        checkpoint
    }

    fn restore_after_failed_destroy(&self, checkpoint: FrontendReadinessCheckpoint) {
        self.generation
            .store(checkpoint.generation, Ordering::Release);
        self.ready_generation
            .store(checkpoint.ready_generation, Ordering::Release);
        if let Ok(mut started) = self.destroy_started.lock() {
            *started = None;
        }
        log_diagnostic(format!(
            "main window generation restored after failed destroy generation={}",
            checkpoint.generation
        ));
    }

    fn mark_ready(&self) -> Option<u64> {
        let generation = self.generation();
        if generation == 0 {
            return None;
        }
        self.ready_generation.store(generation, Ordering::Release);
        Some(generation)
    }

    fn is_ready(&self, generation: u64) -> bool {
        generation != 0 && self.generation() == generation && self.ready_generation() == generation
    }

    /// Returns true when this call newly arms the watchdog for the generation.
    fn try_arm_watchdog(&self, generation: u64) -> bool {
        if generation == 0 || self.is_ready(generation) {
            return false;
        }
        let previous = self
            .watchdog_armed_generation
            .swap(generation, Ordering::AcqRel);
        previous != generation
    }
}

fn watchdog_should_fire(
    armed_generation: u64,
    current_generation: u64,
    ready_generation: u64,
) -> bool {
    armed_generation != 0
        && armed_generation == current_generation
        && ready_generation != armed_generation
}

fn ensure_foreground_microphone_test(state: &WindowLifecycleState) -> Result<(), String> {
    if state.snapshot().mode == WindowLifecycleMode::Foreground {
        Ok(())
    } else {
        Err("microphone test is unavailable while Clipline is backgrounded".into())
    }
}

#[derive(serde::Serialize)]
struct FrontendReadyResponse {
    warnings: Vec<String>,
    window_lifecycle: WindowLifecycleSnapshot,
}

#[derive(serde::Serialize)]
struct DisplayInfo {
    id: String,
    name: String,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    is_primary: bool,
}

#[derive(serde::Serialize)]
struct AudioDeviceInfo {
    id: String,
    name: String,
    is_default: bool,
}

#[derive(serde::Serialize)]
struct AudioDeviceLists {
    outputs: Vec<AudioDeviceInfo>,
    inputs: Vec<AudioDeviceInfo>,
}

#[derive(serde::Serialize, Clone)]
struct GameDetectionEvent {
    active: bool,
    name: Option<String>,
    window_title: Option<String>,
    process_id: Option<u32>,
    process_instance_id: Option<String>,
    exe_name: Option<String>,
    recording_mode: Option<GameRecordingMode>,
    elevated_hotkeys_blocked: bool,
}

#[derive(serde::Serialize)]
struct UpdateCheckResult {
    channel: UpdateChannel,
    channel_label: &'static str,
    current_version: String,
    available: bool,
    version: Option<String>,
    date: Option<String>,
    notes: Option<String>,
    endpoint: &'static str,
    status: Option<String>,
}

impl GameDetectionEvent {
    fn from_detected(detected: Option<&DetectedGame>) -> Self {
        Self::from_detected_with_process_queries(
            detected,
            crate::windows::current_process_is_elevated(),
            crate::windows::process_is_elevated,
            crate::windows::process_instance_id,
        )
    }

    #[cfg(test)]
    fn from_detected_with_elevation(
        detected: Option<&DetectedGame>,
        clipline_elevated: Result<bool, String>,
        game_is_elevated: impl FnOnce(u32) -> Result<bool, String>,
    ) -> Self {
        Self::from_detected_with_process_queries(
            detected,
            clipline_elevated,
            game_is_elevated,
            |process_id| Ok(format!("{process_id}:test")),
        )
    }

    fn from_detected_with_process_queries(
        detected: Option<&DetectedGame>,
        clipline_elevated: Result<bool, String>,
        game_is_elevated: impl FnOnce(u32) -> Result<bool, String>,
        process_instance_id: impl FnOnce(u32) -> Result<String, String>,
    ) -> Self {
        match detected {
            Some(game) => {
                let elevated_hotkeys_blocked = matches!(clipline_elevated, Ok(false))
                    && game_is_elevated(game.process_id).unwrap_or(true);
                let process_instance_id = elevated_hotkeys_blocked.then(|| {
                    process_instance_id(game.process_id)
                        .unwrap_or_else(|_| format!("{}:window:{}", game.process_id, game.hwnd))
                });
                Self {
                    active: true,
                    name: Some(game.name.clone()),
                    window_title: Some(game.window_title.clone()),
                    process_id: Some(game.process_id),
                    process_instance_id,
                    exe_name: Some(game.exe_name.clone()),
                    recording_mode: Some(game.recording_mode),
                    elevated_hotkeys_blocked,
                }
            }
            None => Self {
                active: false,
                name: None,
                window_title: None,
                process_id: None,
                process_instance_id: None,
                exe_name: None,
                recording_mode: None,
                elevated_hotkeys_blocked: false,
            },
        }
    }
}

fn should_log_window_event(event: &WindowEvent) -> bool {
    !matches!(event, WindowEvent::Moved(_) | WindowEvent::Resized(_))
}

fn should_reconcile_native_window_event(event: &WindowEvent) -> bool {
    matches!(event, WindowEvent::Focused(_) | WindowEvent::Resized(_))
}

fn configure_bundled_ffmpeg<R: Runtime>(app: &tauri::App<R>) {
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

fn result_debug<T, E>(result: Result<T, E>) -> String
where
    T: std::fmt::Debug,
    E: std::fmt::Display,
{
    match result {
        Ok(value) => format!("ok({value:?})"),
        Err(e) => format!("err({e})"),
    }
}

fn webview_labels<R: Runtime>(app: &AppHandle<R>) -> String {
    let mut labels = app.webview_windows().into_keys().collect::<Vec<_>>();
    labels.sort();
    format!("[{}]", labels.join(","))
}

fn is_app_window_label(label: &str) -> bool {
    label == MAIN_WINDOW_LABEL
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // exercised by unit tests; open path uses MainWindowOpenAction
enum MainWindowOpenTarget {
    ExistingMain,
    NewMain,
}

#[allow(dead_code)] // exercised by unit tests; open path uses MainWindowOpenAction
fn main_window_open_target(main_window_present: bool) -> MainWindowOpenTarget {
    if main_window_present {
        MainWindowOpenTarget::ExistingMain
    } else {
        MainWindowOpenTarget::NewMain
    }
}

/// Destroy-aware open decision for the tray shell. Distinct from
/// [`MainWindowOpenTarget`] so queued opens during `Destroying` are expressible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MainWindowOpenAction {
    RevealExisting,
    QueueOpen,
    BuildNew,
    Noop,
}

/// Pure tray-shell state used to pin the destroy -> open race without a live
/// Tauri runtime. Production close-to-tray/open paths drive these helpers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MainWindowShellState {
    mode: WindowLifecycleMode,
    main_window_present: bool,
    pending_open: bool,
}

impl MainWindowShellState {
    #[allow(dead_code)] // unit-test constructor; production uses main_window_shell_state
    fn new(mode: WindowLifecycleMode, main_window_present: bool) -> Self {
        Self {
            mode,
            main_window_present,
            pending_open: false,
        }
    }
}

/// Enter `Destroying` while the dying label may still be registered.
fn begin_close_to_tray_destroy(state: &mut MainWindowShellState) {
    state.mode = WindowLifecycleMode::Destroying;
}

/// Queue while `Destroying`; build only when destroyed/absent; never reveal a
/// dying label.
fn request_main_window_open(state: &mut MainWindowShellState) -> MainWindowOpenAction {
    match state.mode {
        WindowLifecycleMode::Destroying => {
            state.pending_open = true;
            MainWindowOpenAction::QueueOpen
        }
        WindowLifecycleMode::Destroyed => {
            state.pending_open = false;
            MainWindowOpenAction::BuildNew
        }
        _ if state.main_window_present => MainWindowOpenAction::RevealExisting,
        _ => MainWindowOpenAction::BuildNew,
    }
}

/// Mark `Destroyed`, clear the label, and build once when an open was queued
/// mid-destroy.
fn observe_main_window_destroyed(state: &mut MainWindowShellState) -> MainWindowOpenAction {
    state.mode = WindowLifecycleMode::Destroyed;
    state.main_window_present = false;
    if state.pending_open {
        state.pending_open = false;
        MainWindowOpenAction::BuildNew
    } else {
        MainWindowOpenAction::Noop
    }
}

/// `Destroying`/`Destroyed` never resolve to `ExistingMain`, even when a stale
/// label is still registered.
#[allow(dead_code)] // unit-test helper; production branches on MainWindowOpenAction
fn main_window_open_target_for(
    mode: WindowLifecycleMode,
    main_window_present: bool,
) -> MainWindowOpenTarget {
    match mode {
        WindowLifecycleMode::Destroying | WindowLifecycleMode::Destroyed => {
            MainWindowOpenTarget::NewMain
        }
        _ => main_window_open_target(main_window_present),
    }
}

fn main_window_shell_state<R: Runtime>(app: &AppHandle<R>) -> MainWindowShellState {
    MainWindowShellState {
        mode: app.state::<WindowLifecycleState>().snapshot().mode,
        main_window_present: app.get_webview_window(MAIN_WINDOW_LABEL).is_some(),
        pending_open: app.state::<MainWindowOpenQueue>().pending(),
    }
}

fn persist_main_window_shell_pending<R: Runtime>(app: &AppHandle<R>, state: &MainWindowShellState) {
    app.state::<MainWindowOpenQueue>()
        .set_pending(state.pending_open);
}

fn prepare_frontend_readiness_for_destroy<R: Runtime>(
    app: &AppHandle<R>,
) -> FrontendReadinessCheckpoint {
    let checkpoint = app.state::<FrontendReadinessState>().clear_for_destroy();
    WEBVIEW_REPAIR_NOTICE_SHOWN.store(false, Ordering::Release);
    checkpoint
}

fn window_state_summary<R: Runtime>(window: &WebviewWindow<R>) -> String {
    format!(
        "label={} visible={} minimized={} focused={} outer_position={} outer_size={} inner_size={}",
        window.label(),
        result_debug(window.is_visible()),
        result_debug(window.is_minimized()),
        result_debug(window.is_focused()),
        result_debug(window.outer_position()),
        result_debug(window.outer_size()),
        result_debug(window.inner_size())
    )
}

fn log_window_state<R: Runtime>(context: &str, window: &WebviewWindow<R>) {
    log_diagnostic(format!("{context}: {}", window_state_summary(window)));
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WebviewRepairNoticeReason {
    GetterFailedToReceiveMessage,
    FrontendReadyTimeout,
    OtherGetterError,
}

fn classify_webview_getter_error(error: &tauri::Error) -> WebviewRepairNoticeReason {
    match error {
        tauri::Error::Runtime(tauri_runtime::Error::FailedToReceiveMessage) => {
            WebviewRepairNoticeReason::GetterFailedToReceiveMessage
        }
        _ => WebviewRepairNoticeReason::OtherGetterError,
    }
}

fn should_show_webview_repair_notice(
    reason: WebviewRepairNoticeReason,
    already_shown: bool,
) -> bool {
    !already_shown
        && matches!(
            reason,
            WebviewRepairNoticeReason::GetterFailedToReceiveMessage
                | WebviewRepairNoticeReason::FrontendReadyTimeout
        )
}

fn show_webview_repair_notice_once(reason: WebviewRepairNoticeReason) {
    if !should_show_webview_repair_notice(
        reason,
        WEBVIEW_REPAIR_NOTICE_SHOWN.load(Ordering::Relaxed),
    ) {
        return;
    }
    if WEBVIEW_REPAIR_NOTICE_SHOWN
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
        .is_err()
    {
        return;
    }

    log_diagnostic(format!("webview2 repair notice shown reason={reason:?}"));
    let _ = std::thread::Builder::new()
        .name("clipline-webview2-repair-notice".into())
        .spawn(move || {
            let _ = rfd::MessageDialog::new()
                .set_title("Clipline needs Microsoft WebView2")
                .set_description(
                    "Clipline is running, but the Windows WebView2 runtime did not start. \
Install or repair Microsoft Edge WebView2 Runtime, then reopen Clipline.\n\n\
You can get it from Microsoft: https://developer.microsoft.com/microsoft-edge/webview2/",
                )
                .set_buttons(rfd::MessageButtons::Ok)
                .show();
        });
}

fn probe_webview_after_reveal<R: Runtime>(window: &WebviewWindow<R>, context: &str) {
    match window.is_visible() {
        Ok(visible) => log_diagnostic(format!("{context} health probe is_visible=ok({visible})")),
        Err(e) => {
            let reason = classify_webview_getter_error(&e);
            log_diagnostic(format!(
                "{context} health probe is_visible=err({e}) reason={reason:?}"
            ));
            show_webview_repair_notice_once(reason);
        }
    }
}

fn arm_frontend_ready_watchdog<R: Runtime>(app: &AppHandle<R>, generation: u64) {
    let readiness = app.state::<FrontendReadinessState>();
    if !readiness.try_arm_watchdog(generation) {
        return;
    }

    log_diagnostic(format!(
        "webview readiness watchdog armed generation={generation}"
    ));
    let app = app.clone();
    let _ = std::thread::Builder::new()
        .name("clipline-webview-readiness-watchdog".into())
        .spawn(move || {
            std::thread::sleep(WEBVIEW_READY_TIMEOUT);
            let readiness = app.state::<FrontendReadinessState>();
            if watchdog_should_fire(
                generation,
                readiness.generation(),
                readiness.ready_generation(),
            ) {
                log_diagnostic(format!(
                    "webview readiness watchdog expired before frontend_ready generation={generation}"
                ));
                show_webview_repair_notice_once(WebviewRepairNoticeReason::FrontendReadyTimeout);
            } else {
                log_diagnostic(format!(
                    "webview readiness watchdog settled generation={generation} current={} ready={}",
                    readiness.generation(),
                    readiness.ready_generation()
                ));
            }
        });
}

const WEBVIEW2_RUNTIME_CLIENT_GUID: &str = "{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}";

fn webview2_runtime_registry_keys() -> [String; 3] {
    [
        format!(
            r"HKLM\SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{WEBVIEW2_RUNTIME_CLIENT_GUID}"
        ),
        format!(r"HKLM\SOFTWARE\Microsoft\EdgeUpdate\Clients\{WEBVIEW2_RUNTIME_CLIENT_GUID}"),
        format!(r"HKCU\Software\Microsoft\EdgeUpdate\Clients\{WEBVIEW2_RUNTIME_CLIENT_GUID}"),
    ]
}

fn parse_reg_pv_output(output: &str) -> Option<String> {
    output.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let name = fields.next()?;
        let kind = fields.next()?;
        if !name.eq_ignore_ascii_case("pv") || !kind.eq_ignore_ascii_case("REG_SZ") {
            return None;
        }
        let value = fields.collect::<Vec<_>>().join(" ");
        (!value.is_empty()).then_some(value)
    })
}

fn query_registry_pv(key: &str) -> Option<String> {
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let output = std::process::Command::new("reg.exe")
        .args(["query", key, "/v", "pv"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_reg_pv_output(&String::from_utf8_lossy(&output.stdout))
}

fn webview2_runtime_diagnostic() -> String {
    let entries = webview2_runtime_registry_keys()
        .into_iter()
        .map(|key| {
            let version = query_registry_pv(&key).unwrap_or_else(|| "missing".to_string());
            format!("{key}={version}")
        })
        .collect::<Vec<_>>();
    format!("webview2_runtime_versions {}", entries.join("; "))
}

#[tauri::command]
async fn memory_status(
    sampler: tauri::State<'_, crate::memory::MemorySampler>,
) -> Result<crate::memory::MemoryStatus, String> {
    sampler.sample().await
}

#[tauri::command]
fn frontend_ready<R: Runtime>(
    app: AppHandle<R>,
    runtime: tauri::State<RuntimeState>,
    startup_warnings: tauri::State<StartupWarnings>,
    window_lifecycle: tauri::State<WindowLifecycleState>,
    readiness: tauri::State<FrontendReadinessState>,
) -> FrontendReadyResponse {
    let generation = readiness.mark_ready();
    log_diagnostic(format!(
        "frontend_ready received generation={} webviews={}",
        generation
            .map(|value| value.to_string())
            .unwrap_or_else(|| "none".into()),
        webview_labels(&app)
    ));
    if let Some(status) = runtime.durable_recorder_status_for_replay() {
        let _ = app.emit("status", status);
    }
    if let Some(game) = runtime.current_game_detection_for_replay() {
        let _ = app.emit("game-detection", game);
    }
    if let Some(event) = runtime.durable_quota_event_for_replay() {
        let _ = app.emit("storage-quota-full", event);
    }
    FrontendReadyResponse {
        warnings: startup_warnings.snapshot(),
        window_lifecycle: window_lifecycle.snapshot(),
    }
}

#[derive(Default)]
struct StartupWarnings(Mutex<Vec<String>>);

impl StartupWarnings {
    fn new(warnings: Vec<String>) -> Self {
        Self(Mutex::new(warnings))
    }

    fn snapshot(&self) -> Vec<String> {
        match self.0.lock() {
            Ok(warnings) => warnings.clone(),
            Err(error) => vec![format!(
                "startup diagnostics could not be read because their lock was poisoned: {error}"
            )],
        }
    }
}

#[derive(serde::Serialize, Clone)]
// Tauri events are JSON, so the live monitor keeps 30 ms chunks as compact
// i16 samples instead of shipping f32 PCM through IPC.
struct MicMonitorEvent {
    rms: f32,
    peak: f32,
    sample_count: usize,
    samples: Vec<i16>,
}

#[derive(Default)]
struct NativeMediaFolderAuthorization(Mutex<Option<PathBuf>>);

impl NativeMediaFolderAuthorization {
    fn authorize(&self, path: PathBuf) {
        if let Ok(mut pending) = self.0.lock() {
            *pending = Some(path);
        }
    }

    fn validate_change(&self, current: &Path, requested: &Path) -> Result<(), String> {
        if same_path(current, requested) {
            return Ok(());
        }
        let pending = self
            .0
            .lock()
            .map_err(|_| "native media-folder authorization is unavailable".to_string())?;
        if pending
            .as_deref()
            .is_some_and(|authorized| same_path(authorized, requested))
        {
            Ok(())
        } else {
            Err("choose a new media folder with the native folder picker first".into())
        }
    }

    fn commit(&self, path: &Path) {
        if let Ok(mut pending) = self.0.lock() {
            if pending
                .as_deref()
                .is_some_and(|authorized| same_path(authorized, path))
            {
                *pending = None;
            }
        }
    }
}

fn same_path(left: &Path, right: &Path) -> bool {
    crate::settings::validation::same_or_nested_path(left, right)
        && crate::settings::validation::same_or_nested_path(right, left)
}

fn display_media_folder_path(path: &Path) -> String {
    let path = path.to_string_lossy();
    let lowercase = path.to_ascii_lowercase();
    if lowercase.starts_with(r"\\?\unc\") {
        format!(r"\\{}", &path[8..])
    } else if lowercase.starts_with(r"\\?\") {
        path[4..].to_string()
    } else {
        path.into_owned()
    }
}

#[derive(Default)]
struct MicTestState(Mutex<MicTestInner>);

#[derive(Default)]
struct MicTestInner {
    last_generation: u64,
    active: Option<MicTestSession>,
}

struct MicTestSession {
    generation: u64,
    stop: Sender<()>,
}

impl MicTestState {
    fn begin(&self) -> Result<(u64, Receiver<()>), String> {
        let (stop, receiver) = mpsc::channel();
        let mut inner = self
            .0
            .lock()
            .map_err(|_| "mic test state lock poisoned".to_string())?;
        inner.last_generation = inner.last_generation.wrapping_add(1).max(1);
        let generation = inner.last_generation;
        let previous = inner.active.replace(MicTestSession { generation, stop });
        if let Some(previous) = previous {
            // Sending is non-blocking for this unbounded control channel. Keep
            // replacement and stop notification in one critical section so a
            // concurrent start cannot create an untracked interval.
            let _ = previous.stop.send(());
        }
        Ok((generation, receiver))
    }

    #[cfg(test)]
    fn is_active(&self, generation: u64) -> bool {
        self.0
            .lock()
            .map(|inner| {
                inner
                    .active
                    .as_ref()
                    .is_some_and(|active| active.generation == generation)
            })
            .unwrap_or(false)
    }

    /// Run a publication while this generation still owns the session lock.
    /// Replacement cannot install a newer generation between the ownership
    /// check and the event, which keeps event order authoritative for the UI.
    fn publish_if_active(&self, generation: u64, publish: impl FnOnce()) -> bool {
        let Ok(inner) = self.0.lock() else {
            return false;
        };
        if inner
            .active
            .as_ref()
            .is_none_or(|active| active.generation != generation)
        {
            return false;
        }
        publish();
        true
    }

    fn finish_if_active_with(&self, generation: u64, finish: impl FnOnce()) -> bool {
        let Ok(mut inner) = self.0.lock() else {
            return false;
        };
        if inner
            .active
            .as_ref()
            .is_none_or(|active| active.generation != generation)
        {
            return false;
        }
        inner.active.take();
        finish();
        true
    }

    fn finish_if_active(&self, generation: u64) -> bool {
        self.finish_if_active_with(generation, || {})
    }

    fn stop(&self) {
        match self.0.lock() {
            Ok(mut inner) => {
                if let Some(session) = inner.active.take() {
                    // Receiver gone means the test thread already exited — not an error.
                    let _ = session.stop.send(());
                }
            }
            Err(e) => tracing::error!(event = "mic_test_state_lock_poisoned", error = %e),
        }
    }
}

fn mic_test_should_stop(receiver: &Receiver<()>) -> bool {
    match receiver.try_recv() {
        Ok(()) | Err(TryRecvError::Disconnected) => true,
        Err(TryRecvError::Empty) => false,
    }
}

pub(crate) struct RuntimeState(Mutex<RuntimeInner>);

static CLOUD_SETTINGS_SAVE_LOCK: Mutex<()> = Mutex::new(());

struct TrayItems<R: Runtime> {
    save_item: MenuItem<R>,
}

impl<R: Runtime> TrayItems<R> {
    fn set_hotkey_label(&self, label: &str) -> Result<(), String> {
        self.save_item
            .set_text(save_menu_text(label))
            .map_err(|e| e.to_string())
    }
}

struct RuntimeInner {
    tx: Option<Sender<Cmd>>,
    recording_generation: u64,
    recording_desired: bool,
    manual_full_session_desired: bool,
    settings: AppSettings,
    lol_url: Option<String>,
    active_game: Option<DetectedGame>,
    osu_title_events: Vec<OsuTitleEvent>,
    last_save_request: Option<Instant>,
    /// Codecs WebView2 can decode, reported by the frontend. Drives the
    /// recorder's Automatic selection; H.264 is the always-safe default.
    decodable_codecs: Vec<service::Codec>,
    last_recorder_status: Option<RecorderDiagnosticStatus>,
    last_storage_status: Option<StorageDiagnosticStatus>,
    recent_recorder_error: bool,
    quota_blocked: Option<Event>,
}

#[derive(Clone)]
struct RecorderDiagnosticStatus {
    recording: bool,
    waiting_for_game: bool,
    segments: usize,
    buffered_s: f64,
    buffered_mb: f64,
    full_session: bool,
    encoder: String,
    capture_backend: String,
}

#[derive(Clone)]
struct StorageDiagnosticStatus {
    total_bytes: u64,
    quota_bytes: Option<u64>,
    over_quota: bool,
}

struct PreparedRuntimeRestart {
    settings: AppSettings,
}

struct PreparedServiceRestart {
    old_tx: Option<Sender<Cmd>>,
    replacement: Option<(ServiceOptions, u64)>,
    waiting_for_game: bool,
    waiting_generation: Option<u64>,
}

#[derive(Debug)]
struct CommittedRuntimeRestart<T> {
    old_tx: Option<Sender<Cmd>>,
    replacement: Option<(T, u64)>,
    cleared_active_game: bool,
    waiting_for_game: bool,
    waiting_generation: Option<u64>,
}

fn recorder_should_run(settings: &AppSettings, active_game: Option<&DetectedGame>) -> bool {
    !settings.games.auto_detect || !settings.games.pause_when_no_game || active_game.is_some()
}

fn waiting_for_game_status() -> Event {
    Event::Status {
        recording: false,
        waiting_for_game: true,
        segments: 0,
        buffered_s: 0.0,
        buffered_mb: 0.0,
        full_session: false,
        encoder: String::new(),
        capture_backend: String::new(),
    }
}

fn emit_waiting_for_game<R: Runtime>(app: &AppHandle<R>) {
    let _ = app.emit("status", waiting_for_game_status());
}

impl RuntimeState {
    fn new(settings: AppSettings, lol_url: Option<String>) -> Self {
        Self::from_parts(None, settings, lol_url)
    }

    #[cfg(test)]
    fn with_sender(tx: Sender<Cmd>, settings: AppSettings, lol_url: Option<String>) -> Self {
        Self::from_parts(Some(tx), settings, lol_url)
    }

    fn from_parts(tx: Option<Sender<Cmd>>, settings: AppSettings, lol_url: Option<String>) -> Self {
        let mut inner = RuntimeInner {
            tx: None,
            recording_generation: 0,
            recording_desired: false,
            manual_full_session_desired: false,
            settings,
            lol_url,
            active_game: None,
            osu_title_events: Vec::new(),
            last_save_request: None,
            decodable_codecs: vec![service::Codec::H264],
            last_recorder_status: None,
            last_storage_status: None,
            recent_recorder_error: false,
            quota_blocked: None,
        };
        if let Some(tx) = tx {
            Self::install_recording_sender(&mut inner, tx);
        }
        Self(Mutex::new(inner))
    }

    fn install_recording_sender(inner: &mut RuntimeInner, tx: Sender<Cmd>) -> u64 {
        inner.recording_generation = inner.recording_generation.wrapping_add(1);
        inner.recording_desired = true;
        inner.tx = Some(tx);
        inner.last_save_request = None;
        inner.recording_generation
    }

    fn accept_service_status(&self, generation: u64, recording: bool) -> bool {
        let Ok(mut inner) = self.0.lock() else {
            return false;
        };
        if inner.recording_generation != generation || inner.tx.is_none() {
            return false;
        }
        if !recording {
            inner.tx = None;
            if inner.quota_blocked.is_none() {
                inner.recording_desired = false;
                inner.manual_full_session_desired = false;
            }
            inner.recording_generation = inner.recording_generation.wrapping_add(1);
            inner.last_save_request = None;
        }
        true
    }

    fn accept_service_quota(&self, generation: u64, event: &Event) -> bool {
        let Event::StorageQuotaFull {
            total_bytes,
            quota_bytes,
            ..
        } = event
        else {
            return false;
        };
        let Ok(mut inner) = self.0.lock() else {
            return false;
        };
        if inner.recording_generation != generation || inner.tx.is_none() {
            return false;
        }
        inner.quota_blocked = Some(event.clone());
        inner.last_storage_status = Some(StorageDiagnosticStatus {
            total_bytes: *total_bytes,
            quota_bytes: Some(*quota_bytes),
            over_quota: true,
        });
        true
    }

    fn observe_runtime_event(&self, event: &Event) {
        let Ok(mut inner) = self.0.lock() else {
            return;
        };
        match event {
            Event::Status {
                recording,
                waiting_for_game,
                segments,
                buffered_s,
                buffered_mb,
                full_session,
                encoder,
                capture_backend,
            } => {
                inner.last_recorder_status = Some(RecorderDiagnosticStatus {
                    recording: *recording,
                    waiting_for_game: *waiting_for_game,
                    segments: *segments,
                    buffered_s: *buffered_s,
                    buffered_mb: *buffered_mb,
                    full_session: *full_session,
                    encoder: encoder.clone(),
                    capture_backend: capture_backend.clone(),
                });
                if *recording {
                    inner.recent_recorder_error = false;
                }
            }
            Event::Saved {
                storage_total_bytes,
                storage_quota_bytes,
                storage_over_quota,
                ..
            } => {
                inner.last_storage_status = Some(StorageDiagnosticStatus {
                    total_bytes: *storage_total_bytes,
                    quota_bytes: *storage_quota_bytes,
                    over_quota: *storage_over_quota,
                });
            }
            Event::StorageQuotaFull { .. } => {}
            Event::Error { .. } => inner.recent_recorder_error = true,
            Event::MediaRootResolved { .. } => {}
        }
    }

    fn current_waiting_status(&self) -> Option<Event> {
        let inner = self.0.lock().ok()?;
        (inner.recording_desired
            && inner.tx.is_none()
            && inner.quota_blocked.is_none()
            && !inner.manual_full_session_desired
            && !recorder_should_run(&inner.settings, inner.active_game.as_ref()))
        .then(waiting_for_game_status)
    }

    /// Prefer the live waiting-for-game state; otherwise replay the last durable
    /// recorder status so a recreated UI can rehydrate without waiting for the
    /// next service tick.
    fn durable_recorder_status_for_replay(&self) -> Option<Event> {
        if let Some(waiting) = self.current_waiting_status() {
            return Some(waiting);
        }
        let inner = self.0.lock().ok()?;
        let status = inner.last_recorder_status.as_ref()?;
        Some(Event::Status {
            recording: status.recording,
            waiting_for_game: status.waiting_for_game,
            segments: status.segments,
            buffered_s: status.buffered_s,
            buffered_mb: status.buffered_mb,
            full_session: status.full_session,
            encoder: status.encoder.clone(),
            capture_backend: status.capture_backend.clone(),
        })
    }

    fn current_game_detection_for_replay(&self) -> Option<GameDetectionEvent> {
        let detected = self.0.lock().ok()?.active_game.clone();
        Some(GameDetectionEvent::from_detected(detected.as_ref()))
    }

    fn durable_quota_event_for_replay(&self) -> Option<Event> {
        self.0.lock().ok()?.quota_blocked.clone()
    }

    fn waiting_generation_is_current(&self, generation: u64) -> bool {
        self.0.lock().is_ok_and(|inner| {
            inner.recording_generation == generation
                && inner.recording_desired
                && inner.tx.is_none()
                && !inner.manual_full_session_desired
                && !recorder_should_run(&inner.settings, inner.active_game.as_ref())
        })
    }

    /// Replace the decodable-codec set from the frontend's canPlayType probe.
    /// Unknown keys are ignored; H.264 is always retained as the safe floor.
    fn set_decodable_codecs(&self, keys: &[String]) {
        let mut codecs = vec![service::Codec::H264];
        for key in keys {
            match key.as_str() {
                "hevc" if !codecs.contains(&service::Codec::Hevc) => {
                    codecs.push(service::Codec::Hevc)
                }
                "av1" if !codecs.contains(&service::Codec::Av1) => codecs.push(service::Codec::Av1),
                _ => {}
            }
        }
        match self.0.lock() {
            Ok(mut inner) => inner.decodable_codecs = codecs,
            Err(e) => tracing::error!(event = "decode_codec_state_lock_poisoned", error = %e),
        }
    }

    /// Build service options for the supplied settings and runtime context.
    fn options_for(
        settings: &AppSettings,
        lol_url: Option<String>,
        active_game: Option<&DetectedGame>,
        decodable_codecs: &[service::Codec],
    ) -> Result<service::ServiceOptions, String> {
        let mut opts = settings.to_service_options(lol_url)?;
        opts.decodable_codecs = decodable_codecs.to_vec();
        if let Some(game) = active_game {
            opts.capture_source = service::CaptureSource::WindowHandle {
                hwnd: game.hwnd,
                title: game.window_title.clone(),
            };
            opts.recording_mode = game.recording_mode.into();
            opts.active_game = Some(service::ActiveGame {
                identity: game.identity.clone(),
                name: game.name.clone(),
                exe_path: game.exe_path.as_deref().map(PathBuf::from),
            });
        }
        Ok(opts)
    }

    fn options(inner: &RuntimeInner) -> Result<service::ServiceOptions, String> {
        let mut options = Self::options_for(
            &inner.settings,
            inner.lol_url.clone(),
            inner.active_game.as_ref(),
            &inner.decodable_codecs,
        )?;
        if inner.manual_full_session_desired {
            options.recording_mode = service::RecordingMode::FullSession;
        }
        Ok(options)
    }

    fn prepare_service_restart(inner: &mut RuntimeInner) -> Result<PreparedServiceRestart, String> {
        let should_run = inner.recording_desired
            && inner.quota_blocked.is_none()
            && (inner.manual_full_session_desired
                || recorder_should_run(&inner.settings, inner.active_game.as_ref()));
        let next_options = if should_run {
            let mut options = match Self::options(inner) {
                Ok(options) => options,
                Err(error) => {
                    // A sender means the current service is still authoritative,
                    // so preserve it on an option error. With no sender, a prior
                    // restart is already spawning; invalidate that stale plan.
                    if inner.tx.is_none() {
                        inner.recording_generation = inner.recording_generation.wrapping_add(1);
                    }
                    return Err(error);
                }
            };
            options.recover_abandoned_recordings = false;
            Some(options)
        } else {
            None
        };
        let old_tx = inner.tx.take();
        inner.recording_generation = inner.recording_generation.wrapping_add(1);
        let generation = inner.recording_generation;
        inner.last_save_request = None;
        Ok(PreparedServiceRestart {
            old_tx,
            replacement: next_options.map(|options| (options, generation)),
            waiting_for_game: inner.recording_desired
                && inner.quota_blocked.is_none()
                && !should_run,
            waiting_generation: (inner.recording_desired
                && inner.quota_blocked.is_none()
                && !should_run)
                .then_some(generation),
        })
    }

    fn arm_manual_session_unless_blocked(inner: &mut RuntimeInner) -> Option<Event> {
        if let Some(event) = inner.quota_blocked.clone() {
            return Some(event);
        }
        inner.manual_full_session_desired = true;
        inner.recording_desired = true;
        inner.last_save_request = None;
        None
    }

    fn prepare_manual_session_stop(
        inner: &mut RuntimeInner,
    ) -> Result<(Option<Sender<Cmd>>, Option<PreparedServiceRestart>), String> {
        inner.manual_full_session_desired = false;
        if inner.recording_desired
            && inner.quota_blocked.is_none()
            && !recorder_should_run(&inner.settings, inner.active_game.as_ref())
        {
            let restart = Self::prepare_service_restart(inner)?;
            let session_tx = restart.old_tx.clone();
            Ok((session_tx, Some(restart)))
        } else {
            Ok((inner.tx.clone(), None))
        }
    }

    fn install_prepared_service_restart(
        inner: &mut RuntimeInner,
        generation: u64,
        tx: Sender<Cmd>,
    ) -> Result<u64, Sender<Cmd>> {
        if !inner.recording_desired
            || inner.quota_blocked.is_some()
            || inner.recording_generation != generation
            || inner.tx.is_some()
        {
            return Err(tx);
        }
        Ok(Self::install_recording_sender(inner, tx))
    }

    fn finish_service_restart<R: Runtime>(
        &self,
        app: AppHandle<R>,
        prepared: PreparedServiceRestart,
    ) -> Result<(), String> {
        let waiting_for_game = prepared.waiting_for_game;
        let waiting_generation = prepared.waiting_generation;
        if let Some(tx) = prepared.old_tx {
            let _ = tx.send(Cmd::Stop { announce: false });
        }
        if let Some((options, restart_generation)) = prepared.replacement {
            let (tx, rx) = service::spawn(options);
            let installed = {
                let mut inner = self.0.lock().map_err(|_| "runtime state lock poisoned")?;
                Self::install_prepared_service_restart(&mut inner, restart_generation, tx)
            };
            match installed {
                Ok(generation) => pump_events(app.clone(), rx, generation),
                Err(tx) => {
                    let _ = tx.send(Cmd::Stop { announce: false });
                }
            }
        }
        if waiting_for_game
            && waiting_generation
                .is_some_and(|generation| self.waiting_generation_is_current(generation))
        {
            emit_waiting_for_game(&app);
        }
        Ok(())
    }

    fn prepare_settings_restart(
        &self,
        settings: AppSettings,
    ) -> Result<PreparedRuntimeRestart, String> {
        let inner = self.0.lock().map_err(|_| "runtime state lock poisoned")?;
        let cleared_active_game = inner.active_game.is_some()
            && !active_game_still_configured(&settings, inner.active_game.as_ref());
        let active_game = if cleared_active_game {
            None
        } else {
            inner.active_game.as_ref()
        };
        if inner.recording_desired
            && inner.quota_blocked.is_none()
            && (inner.manual_full_session_desired || recorder_should_run(&settings, active_game))
        {
            Self::options_for(
                &settings,
                inner.lol_url.clone(),
                active_game,
                &inner.decodable_codecs,
            )?;
        }
        Ok(PreparedRuntimeRestart { settings })
    }

    fn commit_prepared_restart_with<T, F>(
        inner: &mut RuntimeInner,
        prepared: PreparedRuntimeRestart,
        spawn: F,
    ) -> Result<CommittedRuntimeRestart<T>, String>
    where
        F: FnOnce(ServiceOptions) -> (Sender<Cmd>, T),
    {
        let PreparedRuntimeRestart { settings } = prepared;
        let cleared_active_game = inner.active_game.is_some()
            && !active_game_still_configured(&settings, inner.active_game.as_ref());
        let active_game = if cleared_active_game {
            None
        } else {
            inner.active_game.as_ref()
        };
        let should_run = inner.recording_desired
            && inner.quota_blocked.is_none()
            && (inner.manual_full_session_desired || recorder_should_run(&settings, active_game));
        let next_options = if should_run {
            let mut options = Self::options_for(
                &settings,
                inner.lol_url.clone(),
                active_game,
                &inner.decodable_codecs,
            )?;
            if inner.manual_full_session_desired {
                options.recording_mode = service::RecordingMode::FullSession;
            }
            options.recover_abandoned_recordings = false;
            Some(options)
        } else {
            None
        };

        inner.settings = settings;
        if cleared_active_game {
            inner.active_game = None;
        }
        let old_tx = inner.tx.take();
        let replacement = if let Some(options) = next_options {
            let (tx, spawned) = spawn(options);
            let generation = Self::install_recording_sender(inner, tx);
            Some((spawned, generation))
        } else {
            None
        };
        let waiting_for_game =
            inner.recording_desired && inner.quota_blocked.is_none() && !should_run;
        if waiting_for_game {
            inner.recording_generation = inner.recording_generation.wrapping_add(1);
            inner.last_save_request = None;
        }
        let waiting_generation = waiting_for_game.then_some(inner.recording_generation);

        Ok(CommittedRuntimeRestart {
            old_tx,
            replacement,
            cleared_active_game,
            waiting_for_game,
            waiting_generation,
        })
    }

    fn finish_prepared_restart<R: Runtime>(
        &self,
        app: AppHandle<R>,
        prepared: PreparedRuntimeRestart,
    ) -> Result<(), String> {
        let CommittedRuntimeRestart {
            old_tx,
            replacement,
            cleared_active_game,
            waiting_for_game,
            waiting_generation,
        } = {
            let mut inner = self.0.lock().map_err(|_| "runtime state lock poisoned")?;
            Self::commit_prepared_restart_with(&mut inner, prepared, service::spawn)?
        };
        if let Some(tx) = old_tx {
            let _ = tx.send(Cmd::Stop { announce: false });
        }
        if let Some((rx, generation)) = replacement {
            pump_events(app.clone(), rx, generation);
        }
        if waiting_for_game
            && waiting_generation
                .is_some_and(|generation| self.waiting_generation_is_current(generation))
        {
            emit_waiting_for_game(&app);
        }
        if cleared_active_game {
            let _ = app.emit("game-detection", GameDetectionEvent::from_detected(None));
        }
        Ok(())
    }

    fn request_save(&self) -> bool {
        const DOUBLE_TRIGGER_DEBOUNCE: Duration = Duration::from_millis(150);

        if let Ok(mut inner) = self.0.lock() {
            if inner.quota_blocked.is_some() {
                return false;
            }
            let Some(tx) = inner.tx.as_ref().cloned() else {
                return false;
            };
            let now = Instant::now();
            if inner
                .last_save_request
                .is_some_and(|last| now.duration_since(last) < DOUBLE_TRIGGER_DEBOUNCE)
            {
                return false;
            }
            if tx.send(Cmd::Save).is_ok() {
                inner.last_save_request = Some(now);
                return true;
            }
        }
        false
    }

    fn request_save_or_show_quota<R: Runtime>(&self, app: &AppHandle<R>) -> bool {
        if self.request_save() {
            return true;
        }
        if let Some(event) = self.durable_quota_event_for_replay() {
            let _ = app.emit("storage-quota-full", event);
        }
        false
    }

    fn recheck_storage_quota<R: Runtime>(
        &self,
        app: AppHandle<R>,
        media_dir: &Path,
        quota_bytes: Option<u64>,
        auto_delete: bool,
        announce: bool,
    ) -> Result<bool, String> {
        let (required_bytes, still_stopping) = {
            let inner = self.0.lock().map_err(|_| "runtime state lock poisoned")?;
            match inner.quota_blocked.as_ref() {
                Some(Event::StorageQuotaFull { required_bytes, .. }) => {
                    (*required_bytes, inner.tx.is_some())
                }
                _ => return Ok(true),
            }
        };
        if still_stopping {
            if announce {
                if let Some(event) = self.durable_quota_event_for_replay() {
                    let _ = app.emit("storage-quota-full", event);
                }
            }
            return Ok(false);
        }
        let mut status = clipline_storage::storage_status(media_dir, quota_bytes)
            .map_err(|error| format!("storage status for {media_dir:?}: {error}"))?;
        let over_quota = |status: &clipline_storage::StorageStatus| {
            quota_bytes.is_some_and(|quota| {
                status.total_bytes > quota
                    || required_bytes > quota.saturating_sub(status.total_bytes)
            })
        };
        if over_quota(&status) && auto_delete {
            if let Some(quota) = quota_bytes {
                let target = quota.saturating_sub(required_bytes);
                if let Err(error) = clipline_storage::enforce_quota_with_protection(
                    media_dir,
                    Some(target),
                    None,
                    crate::cloud_upload::is_active_upload_source,
                ) {
                    tracing::warn!(
                        event = "storage_quota_auto_delete_failed",
                        path = ?media_dir,
                        error = %error,
                    );
                }
                status = clipline_storage::storage_status(media_dir, quota_bytes)
                    .map_err(|error| format!("storage status for {media_dir:?}: {error}"))?;
            }
        }
        if over_quota(&status) {
            let event = Event::StorageQuotaFull {
                total_bytes: status.total_bytes,
                quota_bytes: quota_bytes.expect("quota checked above"),
                required_bytes,
            };
            {
                let mut inner = self.0.lock().map_err(|_| "runtime state lock poisoned")?;
                inner.quota_blocked = Some(event.clone());
            }
            if announce {
                let _ = app.emit("storage-quota-full", event);
            }
            return Ok(false);
        }

        let should_restart = {
            let mut inner = self.0.lock().map_err(|_| "runtime state lock poisoned")?;
            inner.quota_blocked = None;
            inner.recording_desired
        };
        let _ = app.emit("storage-quota-resolved", ());
        if should_restart {
            self.start_recording(app)?;
        }
        Ok(true)
    }

    fn send(&self, cmd: Cmd) -> bool {
        if let Ok(inner) = self.0.lock() {
            if let Some(tx) = &inner.tx {
                let _ = tx.send(cmd);
                return true;
            }
        }
        false
    }

    fn osu_title_events_for_window(
        &self,
        start: Option<i64>,
        end: Option<i64>,
    ) -> Vec<OsuTitleEvent> {
        let Some(start) = start else {
            return Vec::new();
        };
        let end = end.unwrap_or_else(unix_now_i64);
        self.0
            .lock()
            .map(|inner| filter_osu_title_events(&inner.osu_title_events, start, end))
            .unwrap_or_default()
    }

    pub(crate) fn settings(&self) -> AppSettings {
        self.0
            .lock()
            .map(|inner| inner.settings.clone())
            .unwrap_or_default()
    }

    pub(crate) fn update_cloud<F>(&self, update: F) -> Result<AppSettings, String>
    where
        F: FnOnce(&mut crate::settings::CloudSettings),
    {
        self.update_cloud_with(update, AppSettings::save)
    }

    fn update_cloud_with<F>(
        &self,
        update: F,
        save: impl FnOnce(&AppSettings) -> Result<(), String>,
    ) -> Result<AppSettings, String>
    where
        F: FnOnce(&mut crate::settings::CloudSettings),
    {
        // Serialize cloud settings saves so concurrent uploads preserve their
        // read-modify-write order without holding runtime state during disk I/O.
        let _save_guard = CLOUD_SETTINGS_SAVE_LOCK
            .lock()
            .map_err(|_| "cloud settings save lock poisoned")?;
        let mut next = self
            .0
            .lock()
            .map_err(|_| "runtime state lock poisoned")?
            .settings
            .clone();
        update(&mut next.cloud);
        next.cloud.normalize();
        save(&next)?;
        let mut inner = self.0.lock().map_err(|_| "runtime state lock poisoned")?;
        inner.settings.cloud = next.cloud;
        Ok(inner.settings.clone())
    }

    pub(crate) fn update_osu<F>(&self, update: F) -> Result<AppSettings, String>
    where
        F: FnOnce(&mut crate::settings::OsuApiSettings),
    {
        self.update_osu_with(update, AppSettings::save)
    }

    fn update_osu_with<F>(
        &self,
        update: F,
        save: impl FnOnce(&AppSettings) -> Result<(), String>,
    ) -> Result<AppSettings, String>
    where
        F: FnOnce(&mut crate::settings::OsuApiSettings),
    {
        let _save_guard = CLOUD_SETTINGS_SAVE_LOCK
            .lock()
            .map_err(|_| "settings save lock poisoned")?;
        let mut next = self
            .0
            .lock()
            .map_err(|_| "runtime state lock poisoned")?
            .settings
            .clone();
        update(&mut next.osu);
        next.osu.normalize();
        save(&next)?;
        let mut inner = self.0.lock().map_err(|_| "runtime state lock poisoned")?;
        inner.settings.osu = next.osu;
        Ok(inner.settings.clone())
    }

    fn lock_cloud_settings_save() -> Result<MutexGuard<'static, ()>, String> {
        CLOUD_SETTINGS_SAVE_LOCK
            .lock()
            .map_err(|_| "cloud settings save lock poisoned".to_string())
    }

    fn active_shortcut_matches(&self, shortcut: &Shortcut) -> bool {
        let Ok(inner) = self.0.lock() else {
            return false;
        };
        inner
            .settings
            .hotkeys()
            .into_iter()
            .filter_map(|raw| parse_global_hotkey(raw).ok().flatten())
            .any(|active| &active == shortcut)
    }

    fn set_recording<R: Runtime>(
        &self,
        app: AppHandle<R>,
        recording: bool,
    ) -> Result<bool, String> {
        if recording {
            self.start_recording(app)
        } else {
            self.stop_recording()
        }
    }

    fn start_recording<R: Runtime>(&self, app: AppHandle<R>) -> Result<bool, String> {
        let (started, blocked) = {
            let mut inner = self.0.lock().map_err(|_| "runtime state lock poisoned")?;
            if inner.tx.is_some() {
                return Ok(true);
            }
            inner.recording_desired = true;
            inner.last_save_request = None;
            if let Some(event) = inner.quota_blocked.clone() {
                (None, Some(event))
            } else if inner.manual_full_session_desired
                || recorder_should_run(&inner.settings, inner.active_game.as_ref())
            {
                let (tx, rx) = service::spawn(Self::options(&inner)?);
                let generation = Self::install_recording_sender(&mut inner, tx);
                (Some((rx, generation)), None)
            } else {
                (None, None)
            }
        };
        if let Some(event) = blocked {
            let _ = app.emit("storage-quota-full", event);
            return Ok(false);
        }
        if let Some((rx, generation)) = started {
            pump_events(app, rx, generation);
        } else if let Some(status) = self.current_waiting_status() {
            let _ = app.emit("status", status);
        }
        Ok(true)
    }

    fn stop_recording(&self) -> Result<bool, String> {
        let tx = {
            let mut inner = self.0.lock().map_err(|_| "runtime state lock poisoned")?;
            inner.recording_desired = false;
            inner.manual_full_session_desired = false;
            inner.recording_generation = inner.recording_generation.wrapping_add(1);
            let tx = inner.tx.take();
            inner.last_save_request = None;
            tx
        };
        if let Some(tx) = tx {
            let _ = tx.send(Cmd::Stop { announce: true });
        }
        Ok(false)
    }

    fn set_session_recording<R: Runtime>(
        &self,
        app: AppHandle<R>,
        recording: bool,
    ) -> Result<bool, String> {
        if !recording {
            let (tx, restart) = {
                let mut inner = self.0.lock().map_err(|_| "runtime state lock poisoned")?;
                Self::prepare_manual_session_stop(&mut inner)?
            };
            let session_stopped = tx
                .map(|tx| tx.send(Cmd::StopFullSession).is_ok())
                .unwrap_or(true);
            if let Some(prepared) = restart {
                self.finish_service_restart(app, prepared)?;
            }
            if !session_stopped {
                return Err("recorder stopped before the session could be finalized".into());
            }
            return Ok(false);
        }

        let (started, existing, blocked) = {
            let mut inner = self.0.lock().map_err(|_| "runtime state lock poisoned")?;
            if let Some(event) = Self::arm_manual_session_unless_blocked(&mut inner) {
                (None, None, Some(event))
            } else if let Some(tx) = inner.tx.clone() {
                (None, Some(tx), None)
            } else {
                let (tx, rx) = service::spawn(Self::options(&inner)?);
                let generation = Self::install_recording_sender(&mut inner, tx);
                (Some((rx, generation)), None, None)
            }
        };
        if let Some(event) = blocked {
            let _ = app.emit("storage-quota-full", event);
            return Ok(false);
        }
        if let Some(tx) = existing {
            tx.send(Cmd::StartFullSession)
                .map_err(|_| "recorder stopped before the session could start")?;
        }
        if let Some((rx, generation)) = started {
            pump_events(app, rx, generation);
        }
        Ok(true)
    }

    fn toggle_session_recording_from_hotkey<R: Runtime>(
        &self,
        app: AppHandle<R>,
    ) -> Result<bool, String> {
        let recording = {
            let inner = self.0.lock().map_err(|_| "runtime state lock poisoned")?;
            inner.manual_full_session_desired
                || (inner.tx.is_some()
                    && inner
                        .last_recorder_status
                        .as_ref()
                        .is_some_and(|status| status.full_session))
        };
        self.set_session_recording(app, !recording)
    }

    fn set_detected_game<R: Runtime>(
        &self,
        app: AppHandle<R>,
        detected: Option<DetectedGame>,
    ) -> Result<(), String> {
        let (prepared_restart, emit_event, event) = {
            let mut inner = self.0.lock().map_err(|_| "runtime state lock poisoned")?;
            let detected =
                detected.filter(|game| active_game_still_configured(&inner.settings, Some(game)));
            let event = GameDetectionEvent::from_detected(detected.as_ref());
            record_osu_title_event(&mut inner, detected.as_ref(), unix_now_i64());
            if same_game_window(inner.active_game.as_ref(), detected.as_ref()) {
                if game_recording_mode_changed(inner.active_game.as_ref(), detected.as_ref()) {
                    inner.active_game = detected;
                    (
                        Some(Self::prepare_service_restart(&mut inner)?),
                        true,
                        event,
                    )
                } else if inner.active_game != detected {
                    inner.active_game = detected;
                    (None, true, event)
                } else {
                    (None, false, event)
                }
            } else {
                inner.active_game = detected;
                (
                    Some(Self::prepare_service_restart(&mut inner)?),
                    true,
                    event,
                )
            }
        };
        if let Some(prepared) = prepared_restart {
            self.finish_service_restart(app.clone(), prepared)?;
        }
        if emit_event {
            let _ = app.emit("game-detection", event);
        }
        Ok(())
    }
}

fn record_osu_title_event(inner: &mut RuntimeInner, detected: Option<&DetectedGame>, unix_s: i64) {
    const MAX_OSU_TITLE_EVENTS: usize = 512;
    let Some(game) = detected else {
        return;
    };
    if !game
        .identity
        .is_built_in_plugin(crate::game_plugins::OSU_ID)
    {
        return;
    }
    let title = game.window_title.trim();
    if title.is_empty() {
        return;
    }
    if inner
        .osu_title_events
        .last()
        .is_some_and(|event| event.title == title)
    {
        return;
    }
    inner.osu_title_events.push(OsuTitleEvent {
        unix_s,
        title: title.to_string(),
    });
    if inner.osu_title_events.len() > MAX_OSU_TITLE_EVENTS {
        let overflow = inner.osu_title_events.len() - MAX_OSU_TITLE_EVENTS;
        inner.osu_title_events.drain(0..overflow);
    }
}

fn filter_osu_title_events(events: &[OsuTitleEvent], start: i64, end: i64) -> Vec<OsuTitleEvent> {
    let start = start - 5;
    let end = end.max(start) + 5;
    events
        .iter()
        .filter(|event| event.unix_s >= start && event.unix_s <= end)
        .cloned()
        .collect()
}

fn preserve_backend_owned_settings_fields(settings: &mut AppSettings, backend: &AppSettings) {
    settings.cloud.host_url = backend.cloud.host_url.clone();
    settings.cloud.public_url = backend.cloud.public_url.clone();
    settings.cloud.connected_user_id = backend.cloud.connected_user_id.clone();
    settings.cloud.connected_username = backend.cloud.connected_username.clone();
    settings.cloud.connected_display_name = backend.cloud.connected_display_name.clone();
    settings.cloud.credential_target = backend.cloud.credential_target.clone();
    settings.cloud.credential_cleanup_targets = backend.cloud.credential_cleanup_targets.clone();
    settings.cloud.uploads = backend.cloud.uploads.clone();
    settings.osu = backend.osu.clone();
}

fn same_game_window(current: Option<&DetectedGame>, next: Option<&DetectedGame>) -> bool {
    match (current, next) {
        (Some(current), Some(next)) => {
            current.identity == next.identity && current.hwnd == next.hwnd
        }
        (None, None) => true,
        _ => false,
    }
}

fn game_recording_mode_changed(
    current: Option<&DetectedGame>,
    next: Option<&DetectedGame>,
) -> bool {
    match (current, next) {
        (Some(current), Some(next)) => current.recording_mode != next.recording_mode,
        _ => false,
    }
}

fn active_game_still_configured(settings: &AppSettings, active: Option<&DetectedGame>) -> bool {
    let Some(active) = active else { return true };
    if !settings.games.auto_detect {
        return false;
    }
    match &active.identity {
        crate::game_identity::GameIdentity::BuiltInPlugin(_) => {
            crate::games::built_in_game_still_configured(&settings.games, &active.identity)
        }
        crate::game_identity::GameIdentity::Custom(id) => settings
            .games
            .custom_games
            .iter()
            .any(|game| game.enabled && game.id == *id),
    }
}

#[tauri::command]
fn save_replay<R: Runtime>(app: AppHandle<R>, state: tauri::State<RuntimeState>) {
    state.request_save_or_show_quota(&app);
}

#[tauri::command]
fn recheck_storage_quota<R: Runtime>(
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
fn restart_as_administrator<R: Runtime>(app: AppHandle<R>) -> Result<bool, String> {
    if crate::windows::current_process_is_elevated()? {
        return Ok(false);
    }
    crate::windows::launch_elevated_after(std::process::id())?;
    quit_app(&app);
    Ok(true)
}

#[tauri::command]
fn get_autostart_status<R: Runtime>(app: AppHandle<R>) -> Result<bool, String> {
    app.autolaunch().is_enabled().map_err(|e| e.to_string())
}

fn set_autostart<R: Runtime>(app: &AppHandle<R>, enabled: bool) -> Result<bool, String> {
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

fn autostart_should_mutate_for_current_build() -> bool {
    autostart_should_mutate_for_build(cfg!(debug_assertions))
}

fn autostart_should_mutate_for_build(debug_build: bool) -> bool {
    !debug_build
}

fn saved_autostart_preference_for_current_build(requested: bool, previous: bool) -> bool {
    saved_autostart_preference_for_build(requested, previous, cfg!(debug_assertions))
}

fn saved_autostart_preference_for_build(
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CloseRequestAction {
    Tray,
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MinimizeRequestAction {
    Taskbar,
    Tray,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NativeWindowReconcileAction {
    None,
    BackgroundTaskbar,
    RestoreTaskbar,
}

fn close_request_action(settings: &AppSettings) -> CloseRequestAction {
    if settings.close_to_tray {
        CloseRequestAction::Tray
    } else {
        CloseRequestAction::Quit
    }
}

fn minimize_request_action(settings: &AppSettings) -> MinimizeRequestAction {
    if settings.minimize_to_tray {
        MinimizeRequestAction::Tray
    } else {
        MinimizeRequestAction::Taskbar
    }
}

fn native_window_reconcile_action(
    mode: WindowLifecycleMode,
    is_minimized: bool,
) -> NativeWindowReconcileAction {
    match (mode, is_minimized) {
        (WindowLifecycleMode::Foreground, true) => NativeWindowReconcileAction::BackgroundTaskbar,
        (WindowLifecycleMode::Taskbar, false) => NativeWindowReconcileAction::RestoreTaskbar,
        _ => NativeWindowReconcileAction::None,
    }
}

/// Brings the OS registrations in line with the configured global shortcuts:
/// registers shortcuts new in `new`, unregisters ones dropped from `old`.
/// A registration failure for a shortcut that was already configured (a
/// retry of one that was unavailable earlier) is returned as a warning; a
/// failure for a newly added or removed shortcut rolls back this call's
/// registrations and aborts.
fn sync_global_hotkeys<E>(
    old: &[Shortcut],
    new: &[Shortcut],
    is_registered: impl Fn(Shortcut) -> bool,
    mut register: impl FnMut(Shortcut) -> Result<(), E>,
    mut unregister: impl FnMut(Shortcut) -> Result<(), E>,
) -> Result<Vec<String>, String>
where
    E: std::fmt::Display,
{
    let mut warnings = Vec::new();
    let mut added = Vec::new();
    for shortcut in new {
        if is_registered(*shortcut) {
            continue;
        }
        match register(*shortcut) {
            Ok(()) => added.push(*shortcut),
            Err(e) if old.contains(shortcut) => {
                warnings.push(format!("global save hotkey still unavailable: {e}"));
            }
            Err(e) => {
                for shortcut in added {
                    let _ = unregister(shortcut);
                }
                return Err(format!("register hotkey: {e}"));
            }
        }
    }
    let mut removed = Vec::new();
    for shortcut in old {
        if new.contains(shortcut) || !is_registered(*shortcut) {
            continue;
        }
        if let Err(e) = unregister(*shortcut) {
            let mut rollback_errors = Vec::new();
            for shortcut in removed.into_iter().rev() {
                if let Err(rollback) = register(shortcut) {
                    rollback_errors.push(format!("re-register {shortcut}: {rollback}"));
                }
            }
            for shortcut in added {
                if let Err(rollback) = unregister(shortcut) {
                    rollback_errors.push(format!("unregister {shortcut}: {rollback}"));
                }
            }
            let mut message = format!("replace hotkey: {e}");
            if !rollback_errors.is_empty() {
                message.push_str(&format!(
                    "; rollback incomplete: {}",
                    rollback_errors.join(", ")
                ));
            }
            return Err(message);
        }
        removed.push(*shortcut);
    }
    Ok(warnings)
}

fn send_main_window_to_tray<R: Runtime>(app: &AppHandle<R>) -> Result<(), String> {
    log_diagnostic(format!(
        "send main window to tray webviews={}",
        webview_labels(app)
    ));

    let mode = app.state::<WindowLifecycleState>().snapshot().mode;
    if mode == WindowLifecycleMode::Destroying {
        log_diagnostic("send-to-tray skipped: already Destroying");
        return Ok(());
    }

    let mut windows = app
        .webview_windows()
        .into_iter()
        .filter(|(label, _)| is_app_window_label(label))
        .collect::<Vec<_>>();
    windows.sort_by(|a, b| a.0.cmp(&b.0));

    if mode == WindowLifecycleMode::Destroyed && windows.is_empty() {
        log_diagnostic("send-to-tray skipped: already Destroyed");
        return Ok(());
    }

    // Strong RAM path: destroy the WebView tree. Taskbar minimize still uses
    // hide/Low for restore latency.
    let readiness_checkpoint = prepare_frontend_readiness_for_destroy(app);
    let mut state = main_window_shell_state(app);
    begin_close_to_tray_destroy(&mut state);
    persist_main_window_shell_pending(app, &state);
    publish_background_window(app, WindowLifecycleMode::Destroying);

    if windows.is_empty() {
        log_diagnostic("send-to-tray no live webview; completing Destroyed immediately");
        return complete_main_window_destroyed(app);
    }

    for (label, window) in windows {
        log_window_state(
            &format!("send-to-tray before destroy label={label}"),
            &window,
        );
        let result = window.destroy();
        log_diagnostic(format!(
            "send-to-tray destroy requested label={label}: {}",
            result_debug(result.as_ref())
        ));
        if let Err(error) = result {
            app.state::<FrontendReadinessState>()
                .restore_after_failed_destroy(readiness_checkpoint);
            app.state::<MainWindowOpenQueue>().set_pending(false);
            publish_window_lifecycle(app, mode);
            return Err(format!("destroy main window {label}: {error}"));
        }
        // Do not assert the label is gone: Tauri queues destruction and
        // WindowEvent::Destroyed completes the transition.
    }
    Ok(())
}

fn complete_main_window_destroyed<R: Runtime>(app: &AppHandle<R>) -> Result<(), String> {
    let remaining = app
        .webview_windows()
        .into_keys()
        .filter(|label| is_app_window_label(label))
        .count();
    if remaining > 0 {
        log_diagnostic(format!(
            "main window Destroyed deferred; remaining app webviews={remaining}"
        ));
        return Ok(());
    }

    let mut state = main_window_shell_state(app);
    // The dying label is gone by definition once Destroyed is observed.
    state.main_window_present = false;
    let action = observe_main_window_destroyed(&mut state);
    persist_main_window_shell_pending(app, &state);
    publish_background_window(app, WindowLifecycleMode::Destroyed);
    log_diagnostic(format!(
        "main window Destroyed pending_open={} action={action:?}",
        app.state::<MainWindowOpenQueue>().pending()
    ));

    match action {
        MainWindowOpenAction::BuildNew => open_main_window(app),
        MainWindowOpenAction::RevealExisting
        | MainWindowOpenAction::QueueOpen
        | MainWindowOpenAction::Noop => Ok(()),
    }
}

fn publish_window_lifecycle<R: Runtime>(
    app: &AppHandle<R>,
    mode: WindowLifecycleMode,
) -> WindowLifecycleSnapshot {
    let snapshot = app.state::<WindowLifecycleState>().transition(mode);
    if let Err(error) = app.emit(WINDOW_LIFECYCLE_EVENT, snapshot) {
        log_diagnostic(format!(
            "window lifecycle emit failed revision={} mode={:?}: {error}",
            snapshot.revision, snapshot.mode
        ));
    }
    snapshot
}

fn publish_background_window<R: Runtime>(app: &AppHandle<R>, mode: WindowLifecycleMode) {
    app.state::<MicTestState>().stop();
    if matches!(
        mode,
        WindowLifecycleMode::Destroying | WindowLifecycleMode::Destroyed
    ) {
        app.state::<crate::library::ClipboardExportState>().cancel();
    }
    publish_window_lifecycle(app, mode);
}

/// Request a WebView2 memory-usage target level for one window, best-effort.
///
/// `with_webview` hands the controller over on the webview thread, so this is
/// fire-and-forget like the visibility calls: the outcome is logged, never
/// propagated. A runtime predating `ICoreWebView2_19` reports `unsupported` and
/// is not an error.
fn request_webview_memory_target<R: Runtime>(
    window: &WebviewWindow<R>,
    label: &str,
    target: crate::windows::MemoryTarget,
) {
    let owned_label = label.to_string();
    let dispatched = window.with_webview(move |webview| {
        let outcome = crate::windows::set_memory_target(&webview.controller(), target);
        let described = match &outcome {
            Ok(true) => "ok".to_string(),
            Ok(false) => "unsupported".to_string(),
            Err(error) => format!("failed: {error}"),
        };
        log_diagnostic(format!(
            "webview memory target label={owned_label} target={target:?}: {described}"
        ));
    });
    if let Err(error) = dispatched {
        log_diagnostic(format!(
            "webview memory target dispatch failed label={label} target={target:?}: {error}"
        ));
    }
}

fn quit_app<R: Runtime>(app: &AppHandle<R>) {
    log_diagnostic("quit app requested");
    app.state::<MicTestState>().stop();
    app.state::<crate::library::ClipboardExportState>().cancel();
    app.state::<RuntimeState>()
        .send(Cmd::Stop { announce: false });
    app.exit(0);
}

fn should_open_on_tray_event(event: &TrayIconEvent) -> bool {
    match event {
        TrayIconEvent::Click {
            button,
            button_state,
            ..
        } => should_open_on_tray_click(*button, *button_state),
        _ => false,
    }
}

fn should_open_on_tray_click(button: MouseButton, button_state: MouseButtonState) -> bool {
    button == MouseButton::Left && button_state == MouseButtonState::Up
}

#[tauri::command]
fn minimize_main_window<R: Runtime>(
    app: AppHandle<R>,
    window: WebviewWindow<R>,
    state: tauri::State<RuntimeState>,
) -> Result<(), String> {
    match minimize_request_action(&state.settings()) {
        MinimizeRequestAction::Taskbar => {
            let label = window.label().to_string();
            hide_main_window(
                || window.minimize(),
                || publish_background_window(&app, WindowLifecycleMode::Taskbar),
                || window.as_ref().hide(),
                || {
                    request_webview_memory_target(
                        &window,
                        &label,
                        crate::windows::MemoryTarget::Low,
                    )
                },
            )
        }
        MinimizeRequestAction::Tray => send_main_window_to_tray(&app),
    }
}

fn restore_taskbar_window<R: Runtime>(
    app: &AppHandle<R>,
    window: &WebviewWindow<R>,
) -> Result<(), String> {
    if app.state::<WindowLifecycleState>().snapshot().mode != WindowLifecycleMode::Taskbar {
        return Ok(());
    }
    let label = window.label().to_string();
    let result = restore_taskbar_webview(
        || request_webview_memory_target(window, &label, crate::windows::MemoryTarget::Normal),
        || window.as_ref().show(),
        || {
            publish_window_lifecycle(app, WindowLifecycleMode::Foreground);
        },
    );
    if result.is_err() {
        request_webview_memory_target(window, &label, crate::windows::MemoryTarget::Low);
    }
    result
}

fn restore_taskbar_webview<E>(
    restore_memory_target: impl FnOnce(),
    show_webview: impl FnOnce() -> Result<(), E>,
    publish_foreground: impl FnOnce(),
) -> Result<(), String>
where
    E: std::fmt::Display,
{
    restore_memory_target();
    show_webview().map_err(|error| error.to_string())?;
    publish_foreground();
    Ok(())
}

fn background_if_native_minimized<E>(
    is_minimized: impl FnOnce() -> Result<bool, E>,
    publish_background: impl FnOnce(),
    hide_webview: impl FnOnce() -> Result<(), E>,
    lower_memory_target: impl FnOnce(),
) -> Result<bool, String>
where
    E: std::fmt::Display,
{
    if !is_minimized().map_err(|error| error.to_string())? {
        return Ok(false);
    }
    publish_background();
    let _ = hide_webview();
    lower_memory_target();
    Ok(true)
}

fn background_native_minimized_window<R: Runtime>(
    app: &AppHandle<R>,
    window: &WebviewWindow<R>,
) -> Result<bool, String> {
    if app.state::<WindowLifecycleState>().snapshot().mode != WindowLifecycleMode::Foreground {
        return Ok(false);
    }
    let label = window.label().to_string();
    background_if_native_minimized(
        || window.is_minimized(),
        || publish_background_window(app, WindowLifecycleMode::Taskbar),
        || window.as_ref().hide(),
        || request_webview_memory_target(window, &label, crate::windows::MemoryTarget::Low),
    )
}

fn reconcile_native_window<R: Runtime>(
    app: &AppHandle<R>,
    window: &WebviewWindow<R>,
) -> Result<(), String> {
    let mode = app.state::<WindowLifecycleState>().snapshot().mode;
    if matches!(
        mode,
        WindowLifecycleMode::Tray
            | WindowLifecycleMode::Destroying
            | WindowLifecycleMode::Destroyed
    ) {
        return Ok(());
    }

    let is_minimized = window.is_minimized().map_err(|error| error.to_string())?;
    match native_window_reconcile_action(mode, is_minimized) {
        NativeWindowReconcileAction::None => Ok(()),
        NativeWindowReconcileAction::BackgroundTaskbar => {
            // Re-query inside the transition so a restore racing this event
            // cannot hide a window that is no longer minimized.
            background_native_minimized_window(app, window).map(|_| ())
        }
        NativeWindowReconcileAction::RestoreTaskbar => restore_taskbar_window(app, window),
    }
}

#[tauri::command]
fn set_recording<R: Runtime>(
    app: AppHandle<R>,
    state: tauri::State<RuntimeState>,
    recording: bool,
) -> Result<bool, String> {
    state.set_recording(app, recording)
}

#[tauri::command]
fn set_session_recording<R: Runtime>(
    app: AppHandle<R>,
    state: tauri::State<RuntimeState>,
    recording: bool,
) -> Result<bool, String> {
    state.set_session_recording(app, recording)
}

/// Whether this build bundles a fixed WebView2 runtime (the "standalone"
/// installer variant). The install mode comes from the Tauri config baked in
/// at compile time, so the answer is a property of the installed binary, not
/// of the machine it runs on.
fn is_standalone_install<R: Runtime>(app: &AppHandle<R>) -> bool {
    matches!(
        app.config().bundle.windows.webview_install_mode,
        tauri::utils::config::WebviewInstallMode::FixedRuntime { .. }
    )
}

async fn check_update_for_channel<R: Runtime>(
    app: &AppHandle<R>,
    channel: UpdateChannel,
) -> Result<(Option<tauri_plugin_updater::Update>, Option<String>), String> {
    if !channel.enabled() {
        return Err(format!("{} updates are not available yet", channel.label()));
    }

    let endpoint = channel
        .endpoint(is_standalone_install(app))
        .parse()
        .map_err(|e| format!("parse update endpoint: {e}"))?;
    let updater = app
        .updater_builder()
        .timeout(Duration::from_secs(20))
        .endpoints(vec![endpoint])
        .map_err(|e| e.to_string())?
        .build()
        .map_err(|e| e.to_string())?;

    match updater.check().await {
        Ok(update) => Ok((update, None)),
        Err(tauri_plugin_updater::Error::ReleaseNotFound) => {
            Ok((None, Some(missing_release_metadata_message(channel))))
        }
        Err(e) => Err(e.to_string()),
    }
}

fn missing_release_metadata_message(channel: UpdateChannel) -> String {
    format!(
        "No {} release metadata is published yet. Publish a {} release first.",
        channel.label(),
        channel.label()
    )
}

#[tauri::command]
async fn check_for_updates<R: Runtime>(
    app: AppHandle<R>,
    state: tauri::State<'_, RuntimeState>,
) -> Result<UpdateCheckResult, String> {
    let settings = state.settings();
    let channel = settings.update_channel;
    let current_version = app.package_info().version.to_string();
    let (update, status) = check_update_for_channel(&app, channel).await?;

    Ok(UpdateCheckResult {
        channel,
        channel_label: channel.label(),
        current_version,
        available: update.is_some(),
        version: update.as_ref().map(|update| update.version.clone()),
        date: update
            .as_ref()
            .and_then(|update| update.date.map(|date| date.to_string())),
        notes: update.as_ref().and_then(|update| update.body.clone()),
        endpoint: channel.endpoint(is_standalone_install(&app)),
        status,
    })
}

#[tauri::command]
async fn install_update<R: Runtime>(
    app: AppHandle<R>,
    state: tauri::State<'_, RuntimeState>,
) -> Result<(), String> {
    let channel = state.settings().update_channel;
    let (update, status) = check_update_for_channel(&app, channel).await?;
    let Some(update) = update else {
        return Err(status.unwrap_or_else(|| "no update is available".into()));
    };

    app.state::<MicTestState>().stop();
    state.send(Cmd::Stop { announce: false });
    update
        .download_and_install(|_, _| {}, || {})
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn get_settings(state: tauri::State<RuntimeState>) -> AppSettings {
    state.settings()
}

#[tauri::command]
fn needs_first_run_setup(state: tauri::State<FirstRunState>) -> bool {
    state.is_pending()
}

async fn choose_folder_dialog(
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
async fn choose_media_folder(
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
async fn choose_replay_cache_folder(
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
fn list_displays() -> Result<Vec<DisplayInfo>, String> {
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
fn list_audio_devices() -> Result<AudioDeviceLists, String> {
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
fn probe_encoders() -> Vec<service::EncoderOption> {
    service::available_encoder_options()
}

#[tauri::command]
fn list_game_windows() -> Vec<GameWindowInfo> {
    crate::games::list_game_windows()
}

#[tauri::command(async)]
fn detect_installed_games(
    existing_custom_games: Vec<CustomGameSettings>,
) -> Vec<DetectedGameCandidate> {
    crate::game_discovery::detect_installed_games(&existing_custom_games)
}

/// Extract an executable's icon as a PNG `data:` URL for the custom-games UI.
/// Returns `None` when the path has no usable icon.
#[tauri::command]
fn extract_window_icon(process_id: u32) -> Option<String> {
    let path = crate::games::list_game_windows()
        .into_iter()
        .find(|window| window.process_id == process_id)?
        .exe_path?;
    crate::game_icon::extract_exe_icon_data_url(&path)
}

#[tauri::command]
fn list_game_plugins() -> Vec<GamePluginInfo> {
    crate::games::game_plugin_catalog()
}

/// The frontend reports which codecs WebView2 can decode (canPlayType) so
/// Automatic selection never records a clip the review player can't show.
/// Takes effect on the next recorder (re)start.
#[tauri::command]
fn report_decode_support(state: tauri::State<RuntimeState>, codecs: Vec<String>) {
    state.set_decodable_codecs(&codecs);
}

#[tauri::command]
fn start_microphone_test<R: Runtime>(
    app: AppHandle<R>,
    state: tauri::State<MicTestState>,
    window_lifecycle: tauri::State<WindowLifecycleState>,
    device_id: Option<String>,
    volume: f64,
    mono: bool,
) -> Result<(), String> {
    ensure_foreground_microphone_test(&window_lifecycle)?;
    let channels = if mono {
        clipline_capture::windows::wasapi::WasapiChannelMode::Mono
    } else {
        clipline_capture::windows::wasapi::WasapiChannelMode::Stereo
    };
    let (generation, stop_rx) = state.begin()?;
    if let Err(error) = ensure_foreground_microphone_test(&window_lifecycle) {
        state.finish_if_active(generation);
        return Err(error);
    }
    let worker_app = app.clone();
    let worker = std::thread::Builder::new()
        .name(format!("clipline-mic-test-{generation}"))
        .spawn(move || {
            let run = || -> Result<(), String> {
                let clock = clipline_capture::clock::RelativeClock::new(
                    clipline_capture::windows::qpc_now_ticks_100ns().map_err(|e| e.to_string())?,
                );
                let mut source =
                    clipline_capture::windows::wasapi::WasapiLoopback::start_microphone(
                        clock,
                        device_id.as_deref(),
                        volume,
                        channels,
                    )
                    .map_err(|e| e.to_string())?;
                loop {
                    if mic_test_should_stop(&stop_rx) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(30));
                    if mic_test_should_stop(&stop_rx) {
                        break;
                    }
                    let chunk = source.poll_monitor_chunk().map_err(|e| e.to_string())?;
                    let samples = chunk
                        .samples
                        .into_iter()
                        .map(|sample| {
                            let scaled = (sample.clamp(-1.0, 1.0) * 32_768.0).round();
                            scaled.clamp(i16::MIN as f32, i16::MAX as f32) as i16
                        })
                        .collect();
                    let mic_state = worker_app.state::<MicTestState>();
                    mic_state.publish_if_active(generation, || {
                        let _ = worker_app.emit(
                            "mic-test",
                            MicMonitorEvent {
                                rms: chunk.level.rms,
                                peak: chunk.level.peak,
                                sample_count: chunk.level.sample_count,
                                samples,
                            },
                        );
                    });
                }
                Ok(())
            };
            if let Err(e) = run() {
                let mic_state = worker_app.state::<MicTestState>();
                mic_state.finish_if_active_with(generation, || {
                    let _ = worker_app.emit("mic-test-error", e);
                    let _ = worker_app.emit("mic-test-stopped", ());
                });
            }
        });
    if let Err(error) = worker {
        state.finish_if_active(generation);
        return Err(format!("could not start microphone test thread: {error}"));
    }
    Ok(())
}

#[tauri::command]
fn stop_microphone_test(state: tauri::State<MicTestState>) {
    state.stop();
}

fn parse_global_hotkey(raw: &str) -> Result<Option<Shortcut>, String> {
    if is_global_shortcut_hotkey(raw)? {
        parse_hotkey(raw).map(Some)
    } else {
        Ok(None)
    }
}

/// The configured Save Replay keybinds that go through the OS global-shortcut
/// registry (mouse and modified keyboard binds use the low-level hook instead).
fn global_hotkeys(settings: &AppSettings) -> Result<Vec<Shortcut>, String> {
    let mut shortcuts = Vec::new();
    for raw in settings.hotkeys() {
        if let Some(shortcut) = parse_global_hotkey(raw)? {
            shortcuts.push(shortcut);
        }
    }
    Ok(shortcuts)
}

fn save_hotkey_label(settings: &AppSettings) -> String {
    settings.hotkeys().join(" / ")
}

#[cfg(test)]
fn run_before_releasing_settings_save_lock<T>(
    save_guard: MutexGuard<'_, ()>,
    operation: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    let result = operation();
    drop(save_guard);
    result
}

#[derive(Default)]
struct AppliedSettingsSideEffects {
    global_hotkeys: bool,
    hook_hotkeys: bool,
    tray_label: bool,
    autostart: bool,
}

fn rollback_settings_side_effects<R: Runtime>(
    app: &AppHandle<R>,
    tray_items: &TrayItems<R>,
    old: &AppSettings,
    old_global_hotkeys: &[Shortcut],
    new_global_hotkeys: &[Shortcut],
    applied: &AppliedSettingsSideEffects,
) -> Vec<String> {
    let mut errors = Vec::new();
    if applied.autostart {
        if let Err(error) = set_autostart(app, old.open_on_startup) {
            errors.push(format!("restore Windows startup registration: {error}"));
        }
    }
    if applied.tray_label {
        if let Err(error) = tray_items.set_hotkey_label(&save_hotkey_label(old)) {
            errors.push(format!("restore tray hotkey label: {error}"));
        }
    }
    if applied.hook_hotkeys {
        if let Err(error) =
            crate::hotkeys::set_hotkeys(&old.hotkeys(), &old.recording_hotkeys())
        {
            errors.push(format!("restore low-level hotkeys: {error}"));
        }
    }
    if applied.global_hotkeys {
        let shortcuts = app.global_shortcut();
        if let Err(error) = sync_global_hotkeys(
            new_global_hotkeys,
            old_global_hotkeys,
            |shortcut| shortcuts.is_registered(shortcut),
            |shortcut| shortcuts.register(shortcut),
            |shortcut| shortcuts.unregister(shortcut),
        ) {
            errors.push(format!("restore global save hotkeys: {error}"));
        }
    }
    errors
}

fn settings_transaction_error(primary: String, rollback_errors: Vec<String>) -> String {
    if rollback_errors.is_empty() {
        primary
    } else {
        format!(
            "{primary}; settings rollback incomplete: {}",
            rollback_errors.join(", ")
        )
    }
}

#[tauri::command]
fn save_settings<R: Runtime>(
    app: AppHandle<R>,
    state: tauri::State<RuntimeState>,
    first_run_state: tauri::State<FirstRunState>,
    tray_items: tauri::State<TrayItems<R>>,
    storage_settings: tauri::State<crate::library::StorageSettings>,
    media_folder_authorization: tauri::State<NativeMediaFolderAuthorization>,
    mut settings: AppSettings,
) -> Result<AppSettings, String> {
    settings.hotkey = crate::settings::normalize_hotkey(&settings.hotkey)?;
    settings.hotkey_secondary = match settings.hotkey_secondary.as_deref() {
        Some(raw) if !raw.trim().is_empty() => Some(crate::settings::normalize_hotkey(raw)?),
        _ => None,
    };
    settings.recording_hotkey = match settings.recording_hotkey.as_deref() {
        Some(raw) if !raw.trim().is_empty() => Some(crate::settings::normalize_hotkey(raw)?),
        _ => None,
    };
    settings.recording_hotkey_secondary = match settings.recording_hotkey_secondary.as_deref() {
        Some(raw) if !raw.trim().is_empty() => Some(crate::settings::normalize_hotkey(raw)?),
        _ => None,
    };
    settings.games.normalize();
    settings.validate()?;
    let media_dir = settings.media_dir_path()?;
    let cloud_save_guard = RuntimeState::lock_cloud_settings_save()?;
    let old = state.settings();
    preserve_backend_owned_settings_fields(&mut settings, &old);
    let old_media_dir = old.media_dir_path()?;
    media_folder_authorization.validate_change(&old_media_dir, &media_dir)?;
    service::prepare_writable_media_directory(&media_dir)?;

    // Apply the autostart registry change before persisting so settings.json
    // can never say "enabled" while the Run key update failed. Debug builds
    // share settings with installed builds, so they preserve this preference
    // and leave the shared Run key alone.
    let requested_open_on_startup = settings.open_on_startup;
    settings.open_on_startup = saved_autostart_preference_for_current_build(
        requested_open_on_startup,
        old.open_on_startup,
    );
    let old_global_hotkeys = global_hotkeys(&old)?;
    let new_global_hotkeys = global_hotkeys(&settings)?;
    let quota_bytes = quota_bytes_from_gb(settings.disk_quota_gb)?;
    let shortcuts = app.global_shortcut();
    let mut applied = AppliedSettingsSideEffects::default();
    let warnings = sync_global_hotkeys(
        &old_global_hotkeys,
        &new_global_hotkeys,
        |shortcut| shortcuts.is_registered(shortcut),
        |shortcut| shortcuts.register(shortcut),
        |shortcut| shortcuts.unregister(shortcut),
    )?;
    applied.global_hotkeys = true;
    if let Err(primary) = crate::hotkeys::set_hotkeys(
        &settings.hotkeys(),
        &settings.recording_hotkeys(),
    ) {
        let rollback = rollback_settings_side_effects(
            &app,
            &tray_items,
            &old,
            &old_global_hotkeys,
            &new_global_hotkeys,
            &applied,
        );
        return Err(settings_transaction_error(primary, rollback));
    }
    applied.hook_hotkeys = true;
    if let Err(primary) = tray_items.set_hotkey_label(&save_hotkey_label(&settings)) {
        let rollback = rollback_settings_side_effects(
            &app,
            &tray_items,
            &old,
            &old_global_hotkeys,
            &new_global_hotkeys,
            &applied,
        );
        return Err(settings_transaction_error(primary, rollback));
    }
    applied.tray_label = true;
    if settings.open_on_startup != old.open_on_startup
        && autostart_should_mutate_for_current_build()
    {
        match set_autostart(&app, settings.open_on_startup) {
            Ok(actual) => {
                settings.open_on_startup = actual;
                applied.autostart = true;
            }
            Err(primary) => {
                let rollback = rollback_settings_side_effects(
                    &app,
                    &tray_items,
                    &old,
                    &old_global_hotkeys,
                    &new_global_hotkeys,
                    &applied,
                );
                return Err(settings_transaction_error(
                    format!("update Windows startup registration: {primary}"),
                    rollback,
                ));
            }
        }
    }
    let prepared_restart = match state.prepare_settings_restart(settings.clone()) {
        Ok(prepared) => prepared,
        Err(primary) => {
            let rollback = rollback_settings_side_effects(
                &app,
                &tray_items,
                &old,
                &old_global_hotkeys,
                &new_global_hotkeys,
                &applied,
            );
            return Err(settings_transaction_error(primary, rollback));
        }
    };
    if let Err(error) = settings.save() {
        let rollback = rollback_settings_side_effects(
            &app,
            &tray_items,
            &old,
            &old_global_hotkeys,
            &new_global_hotkeys,
            &applied,
        );
        return Err(settings_transaction_error(error, rollback));
    }
    if let Err(primary) = state.finish_prepared_restart(app.clone(), prepared_restart) {
        let mut rollback = Vec::new();
        if let Err(error) = old.save() {
            rollback.push(format!("restore settings.json: {error}"));
        }
        rollback.extend(rollback_settings_side_effects(
            &app,
            &tray_items,
            &old,
            &old_global_hotkeys,
            &new_global_hotkeys,
            &applied,
        ));
        return Err(settings_transaction_error(primary, rollback));
    }
    drop(cloud_save_guard);
    for message in warnings {
        tracing::warn!(event = "settings_apply_warning", message = %message);
        let _ = app.emit("error", message);
    }
    storage_settings.set_quota_bytes(quota_bytes);
    storage_settings.set_media_dir(media_dir.clone());
    if let Err(error) = state.recheck_storage_quota(
        app.clone(),
        &media_dir,
        quota_bytes,
        settings.auto_delete_when_over_quota,
        true,
    ) {
        tracing::warn!(event = "storage_quota_recheck_failed", error = %error);
        let _ = app.emit("error", error);
    }
    media_folder_authorization.commit(&media_dir);
    first_run_state.complete();
    Ok(settings)
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
            save_replay,
            recheck_storage_quota,
            restart_as_administrator,
            set_recording,
            set_session_recording,
            get_settings,
            needs_first_run_setup,
            minimize_main_window,
            choose_media_folder,
            choose_replay_cache_folder,
            list_displays,
            list_audio_devices,
            probe_encoders,
            report_decode_support,
            list_game_plugins,
            list_game_windows,
            detect_installed_games,
            extract_window_icon,
            memory_status,
            frontend_ready,
            crate::ffmpeg_install::ffmpeg_runtime_status,
            crate::ffmpeg_install::ensure_ffmpeg_runtime,
            crate::ffmpeg_install::cancel_ffmpeg_runtime_install,
            start_microphone_test,
            stop_microphone_test,
            get_autostart_status,
            check_for_updates,
            install_update,
            save_settings,
            support::prepare_bug_report,
            support::submit_bug_report,
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
            crate::library::export_clip,
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
            if let Err(e) = crate::hotkeys::install_hotkey_hook(
                &settings.hotkeys(),
                &settings.recording_hotkeys(),
                {
                let app = app.handle().clone();
                move |action| match action {
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

fn spawn_game_detector<R: Runtime>(app: AppHandle<R>) {
    std::thread::Builder::new()
        .name("clipline-game-detector".into())
        .spawn(move || {
            let mut last_error = None::<String>;
            loop {
                std::thread::sleep(GAME_DETECTOR_INTERVAL);
                let settings = app.state::<RuntimeState>().settings();
                let detected = crate::games::detect_active_game(&settings.games);
                match app
                    .state::<RuntimeState>()
                    .set_detected_game(app.clone(), detected)
                {
                    Ok(()) => last_error = None,
                    Err(e) if last_error.as_deref() != Some(e.as_str()) => {
                        last_error = Some(e.clone());
                        let _ = app.emit("error", format!("game detection: {e}"));
                    }
                    Err(_) => {}
                }
            }
        })
        .expect("spawn game detector thread");
}

fn open_main_window<R: Runtime>(app: &AppHandle<R>) -> Result<(), String> {
    log_diagnostic(format!(
        "open_main_window start webviews={}",
        webview_labels(app)
    ));

    let mut state = main_window_shell_state(app);
    let action = request_main_window_open(&mut state);
    persist_main_window_shell_pending(app, &state);
    log_diagnostic(format!(
        "open_main_window action={action:?} mode={:?} present={} pending={}",
        state.mode, state.main_window_present, state.pending_open
    ));

    match action {
        MainWindowOpenAction::QueueOpen => {
            log_diagnostic("open_main_window queued until WindowEvent::Destroyed");
            Ok(())
        }
        MainWindowOpenAction::Noop => Ok(()),
        MainWindowOpenAction::RevealExisting => {
            let window = app
                .get_webview_window(MAIN_WINDOW_LABEL)
                .ok_or_else(|| "main window vanished before reveal".to_string())?;
            // A stale registered label during Destroying/Destroyed must never
            // reach here; request_main_window_open queues/builds instead.
            log_window_state("open existing before reveal", &window);
            let result = reveal_logged_window(&window, "open existing");
            log_window_state("open existing after reveal", &window);
            probe_webview_after_reveal(&window, "open existing after reveal");
            arm_frontend_ready_watchdog(app, app.state::<FrontendReadinessState>().generation());
            result
        }
        MainWindowOpenAction::BuildNew => {
            log_diagnostic("open_main_window building main window");
            let window = build_main_window(app, MAIN_WINDOW_LABEL)?;
            log_window_state("open rebuilt before reveal", &window);
            let result = reveal_logged_window(&window, "open rebuilt");
            log_window_state("open rebuilt after reveal", &window);
            probe_webview_after_reveal(&window, "open rebuilt after reveal");
            arm_frontend_ready_watchdog(app, app.state::<FrontendReadinessState>().generation());
            result
        }
    }
}

fn build_main_window<R: Runtime>(
    app: &AppHandle<R>,
    label: &str,
) -> Result<WebviewWindow<R>, String> {
    let mut config = app
        .config()
        .app
        .windows
        .first()
        .ok_or_else(|| "missing main window config".to_string())?
        .clone();
    config.label = label.to_string();
    let window = WebviewWindowBuilder::from_config(app, &config)
        .map_err(|e| e.to_string())?
        .build()
        .map_err(|e| e.to_string())?;
    let generation = app.state::<FrontendReadinessState>().begin_generation();
    log_diagnostic(format!(
        "build_main_window ready label={label} generation={generation} webviews={}",
        webview_labels(app)
    ));
    Ok(window)
}

fn reveal_logged_window<R: Runtime>(
    window: &WebviewWindow<R>,
    context: &str,
) -> Result<(), String> {
    reveal_main_window(
        || request_webview_memory_target(window, context, crate::windows::MemoryTarget::Normal),
        || {
            let result = window.as_ref().show();
            log_diagnostic(format!(
                "{context} webview show: {}",
                result_debug(result.as_ref())
            ));
            result
        },
        || {
            let result = window.show();
            log_diagnostic(format!("{context} show: {}", result_debug(result.as_ref())));
            result
        },
        || {
            let result = window.unminimize();
            log_diagnostic(format!(
                "{context} unminimize: {}",
                result_debug(result.as_ref())
            ));
            result
        },
        || {
            let result = window.set_focus();
            log_diagnostic(format!(
                "{context} set_focus: {}",
                result_debug(result.as_ref())
            ));
            result
        },
        || {
            publish_window_lifecycle(window.app_handle(), WindowLifecycleMode::Foreground);
        },
    )
}

/// Reveal order is load-bearing: the WebView2 controller becomes visible before
/// the native window is shown, so the first painted frame is real content rather
/// than a transparent or stale one.
///
/// Controller visibility is best-effort. A failure there is logged but never
/// propagated — refusing to reveal would leave the window unrecoverable from the
/// tray, which is far worse than rendering while hidden.
fn reveal_main_window<E>(
    restore_memory_target: impl FnOnce(),
    show_webview: impl FnOnce() -> Result<(), E>,
    show: impl FnOnce() -> Result<(), E>,
    unminimize: impl FnOnce() -> Result<(), E>,
    focus: impl FnOnce() -> Result<(), E>,
    publish_foreground: impl FnOnce(),
) -> Result<(), String>
where
    E: std::fmt::Display,
{
    // Normal before anything becomes visible: a view still at Low when it
    // paints would show the throttled frame to the user.
    restore_memory_target();
    let _ = show_webview();
    // Native operations can report a transient error after already changing
    // window state. Attempt every recovery step, and never gate the lifecycle
    // event that boots the frontend on one of those fallible results.
    let show_error = show().err().map(|error| error.to_string());
    let unminimize_error = unminimize().err().map(|error| error.to_string());
    publish_foreground();
    let focus_error = focus().err().map(|error| error.to_string());

    match show_error.or(unminimize_error).or(focus_error) {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// Hide order is the mirror: the native window goes first, so a failed OS hide
/// can never leave a still-visible window with a blanked webview inside it.
///
/// Hiding the controller is what actually releases WebView2's rendering
/// resources — `Webview::hide` reaches `ICoreWebView2Controller::SetIsVisible`
/// through wry, which hiding the host window alone does not do. It is
/// best-effort: by that point the window is already in the tray.
fn hide_main_window<E>(
    hide: impl FnOnce() -> Result<(), E>,
    publish_background: impl FnOnce(),
    hide_webview: impl FnOnce() -> Result<(), E>,
    lower_memory_target: impl FnOnce(),
) -> Result<(), String>
where
    E: std::fmt::Display,
{
    hide().map_err(|e| e.to_string())?;
    publish_background();
    let _ = hide_webview();
    // Only once the window is genuinely gone: throttling a view the user can
    // still see would be visible to them.
    lower_memory_target();
    Ok(())
}

fn pump_events<R: Runtime>(handle: AppHandle<R>, event_rx: Receiver<Event>, generation: u64) {
    std::thread::spawn(move || {
        for event in event_rx {
            if matches!(&event, Event::StorageQuotaFull { .. }) {
                if !handle
                    .state::<RuntimeState>()
                    .accept_service_quota(generation, &event)
                {
                    continue;
                }
            } else {
                handle.state::<RuntimeState>().observe_runtime_event(&event);
            }
            if let Event::MediaRootResolved { path, .. } = &event {
                let media_root = PathBuf::from(path);
                handle
                    .state::<crate::library::StorageSettings>()
                    .set_media_dir(media_root);
            }
            if let Event::Status { recording, .. } = &event {
                let accepted = handle
                    .state::<RuntimeState>()
                    .accept_service_status(generation, *recording);
                if !accepted {
                    continue;
                }
            }
            let _ = match &event {
                Event::MediaRootResolved { .. } => Ok(()),
                Event::Status { .. } => handle.emit("status", &event),
                Event::Saved { .. } => handle.emit("saved", &event),
                Event::StorageQuotaFull { .. } => handle.emit("storage-quota-full", &event),
                Event::Error { message } => handle.emit("error", message.clone()),
            };
            if let Event::Saved {
                full_session: false,
                ..
            } = &event
            {
                crate::sound::play_replay_saved();
            }
            if let Event::Saved {
                path,
                seconds,
                recording_start_unix,
                recording_end_unix,
                full_session: true,
                ..
            } = &event
            {
                let title_events = handle
                    .state::<RuntimeState>()
                    .osu_title_events_for_window(*recording_start_unix, *recording_end_unix);
                let saved = crate::osu_enrichment::OsuSavedClip {
                    path: std::path::PathBuf::from(path),
                    seconds: *seconds,
                    full_session: true,
                    recording_start_unix: *recording_start_unix,
                    recording_end_unix: *recording_end_unix,
                    title_events,
                };
                match crate::osu_enrichment::write_pending_for_saved_clip(&saved) {
                    Ok(Some(_)) => {
                        let app = handle.clone();
                        let media_root = handle
                            .state::<crate::library::StorageSettings>()
                            .media_dir();
                        tauri::async_runtime::spawn(async move {
                            if let Err(e) =
                                crate::osu_api::retry_pending_enrichment(&app, media_root).await
                            {
                                tracing::warn!(event = "save_osu_enrichment_retry_failed", error = %e);
                            }
                        });
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::warn!(event = "osu_enrichment_queue_failed", error = %e);
                    }
                }
            }
        }
    });
}

fn parse_quota_gb(raw: &str) -> Result<Option<u64>, &'static str> {
    let gb = raw.parse::<f64>().map_err(|_| "expected a number of GiB")?;
    if !gb.is_finite() || gb < 0.0 {
        return Err("quota must be a non-negative finite number");
    }
    if gb == 0.0 {
        return Ok(None);
    }
    quota_bytes_from_gb(gb).map_err(|_| "quota is too large")
}

fn save_menu_text(label: &str) -> String {
    format!("Save Replay ({label})")
}

/// Procedural 32x32 tray icon: a recording dot on a dark rounded square —
/// no asset files, no bundler.
fn tray_icon() -> Image<'static> {
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
    use super::*;
    use crate::settings::{
        CloudUploadRecord, GameRecordingMode, ReplayStorageMode, ReplayStorageSettings,
    };

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
    fn startup_warnings_remain_durable_across_frontend_ready_replays() {
        let warnings = StartupWarnings::new(vec!["settings recovered".into()]);

        assert_eq!(warnings.snapshot(), vec!["settings recovered"]);
        assert_eq!(
            warnings.snapshot(),
            vec!["settings recovered"],
            "recreated UIs must see the same durable startup warnings"
        );
    }

    #[test]
    fn frontend_readiness_generations_isolate_watchdogs_and_ready_markers() {
        let readiness = FrontendReadinessState::default();
        assert_eq!(readiness.generation(), 0);

        let first = readiness.begin_generation();
        assert_eq!(first, 1);
        assert!(readiness.try_arm_watchdog(first));
        assert!(!readiness.try_arm_watchdog(first));
        assert_eq!(readiness.mark_ready(), Some(first));
        assert!(readiness.is_ready(first));
        assert!(!readiness.try_arm_watchdog(first));

        readiness.clear_for_destroy();
        assert_eq!(readiness.generation(), 0);
        assert!(!readiness.is_ready(first));

        let second = readiness.begin_generation();
        assert_eq!(second, first + 1);
        assert!(readiness.try_arm_watchdog(second));
        assert!(watchdog_should_fire(
            second,
            readiness.generation(),
            readiness.ready_generation()
        ));
        // An old timer must not fire against a newer window.
        assert!(!watchdog_should_fire(
            first,
            readiness.generation(),
            readiness.ready_generation()
        ));
        assert_eq!(readiness.mark_ready(), Some(second));
        assert!(!watchdog_should_fire(
            second,
            readiness.generation(),
            readiness.ready_generation()
        ));
    }

    #[test]
    fn failed_destroy_restores_the_live_frontend_generation() {
        let readiness = FrontendReadinessState::default();
        let generation = readiness.begin_generation();
        assert_eq!(readiness.mark_ready(), Some(generation));

        let checkpoint = readiness.clear_for_destroy();
        assert_eq!(readiness.generation(), 0);
        readiness.restore_after_failed_destroy(checkpoint);

        assert_eq!(readiness.generation(), generation);
        assert!(readiness.is_ready(generation));
    }

    #[test]
    fn durable_recorder_status_prefers_waiting_then_last_status() {
        let mut settings = AppSettings::default();
        settings.games.pause_when_no_game = true;
        let state = RuntimeState::new(settings, None);
        {
            let mut inner = state.0.lock().unwrap();
            inner.recording_desired = true;
            inner.last_recorder_status = Some(RecorderDiagnosticStatus {
                recording: true,
                waiting_for_game: false,
                segments: 3,
                buffered_s: 12.0,
                buffered_mb: 4.0,
                full_session: false,
                encoder: "mft-h264".into(),
                capture_backend: "wgc".into(),
            });
        }

        assert!(matches!(
            state.durable_recorder_status_for_replay(),
            Some(Event::Status {
                recording: false,
                waiting_for_game: true,
                ..
            })
        ));

        {
            let mut inner = state.0.lock().unwrap();
            inner.recording_desired = false;
        }

        assert!(matches!(
            state.durable_recorder_status_for_replay(),
            Some(Event::Status {
                recording: true,
                waiting_for_game: false,
                segments: 3,
                encoder,
                ..
            }) if encoder == "mft-h264"
        ));
    }

    #[test]
    fn current_game_detection_is_available_for_frontend_replay() {
        let state = RuntimeState::new(AppSettings::default(), None);
        state.0.lock().unwrap().active_game = Some(detected_game("game", "Game", 42));

        let event = state.current_game_detection_for_replay().unwrap();

        assert!(event.active);
        assert_eq!(event.name.as_deref(), Some("Game"));
        assert_eq!(event.window_title.as_deref(), Some("Game Window"));
    }

    #[test]
    fn microphone_test_stop_channel_treats_disconnect_as_shutdown() {
        let (sender, receiver) = mpsc::channel();
        assert!(!mic_test_should_stop(&receiver));
        sender.send(()).unwrap();
        assert!(mic_test_should_stop(&receiver));

        let (sender, receiver) = mpsc::channel();
        drop(sender);
        assert!(mic_test_should_stop(&receiver));
    }

    #[test]
    fn concurrent_microphone_test_starts_leave_one_tracked_generation() {
        const STARTS: usize = 12;
        let state = std::sync::Arc::new(MicTestState::default());
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(STARTS));
        let workers = (0..STARTS)
            .map(|_| {
                let state = state.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    state.begin().expect("session replacement")
                })
            })
            .collect::<Vec<_>>();
        let sessions = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>();

        let active = sessions
            .iter()
            .filter(|(generation, _)| state.is_active(*generation))
            .count();
        assert_eq!(active, 1);
        for (generation, receiver) in &sessions {
            assert_eq!(
                mic_test_should_stop(receiver),
                !state.is_active(*generation),
                "every superseded receiver must observe shutdown"
            );
        }
    }

    #[test]
    fn stale_microphone_test_cannot_publish_or_finish_active_generation() {
        let state = MicTestState::default();
        let (old_generation, _old_receiver) = state.begin().unwrap();
        let (active_generation, _active_receiver) = state.begin().unwrap();
        let published = std::sync::atomic::AtomicUsize::new(0);

        assert!(!state.publish_if_active(old_generation, || {
            published.fetch_add(1, Ordering::Relaxed);
        }));
        assert!(state.publish_if_active(active_generation, || {
            published.fetch_add(1, Ordering::Relaxed);
        }));
        assert_eq!(published.load(Ordering::Relaxed), 1);

        assert!(!state.finish_if_active(old_generation));
        assert!(state.is_active(active_generation));
        assert!(state.finish_if_active(active_generation));
        assert!(!state.is_active(active_generation));
    }

    #[test]
    fn failed_cloud_settings_save_leaves_live_state_unchanged() {
        let state = RuntimeState::new(AppSettings::default(), None);

        let error = state
            .update_cloud_with(
                |cloud| cloud.host_url = "https://new.example".into(),
                |candidate| {
                    assert_eq!(candidate.cloud.host_url, "https://new.example");
                    Err("disk full".into())
                },
            )
            .unwrap_err();

        assert_eq!(error, "disk full");
        assert!(state.settings().cloud.host_url.is_empty());
    }

    #[test]
    fn failed_osu_settings_save_leaves_live_state_unchanged() {
        let state = RuntimeState::new(AppSettings::default(), None);

        let error = state
            .update_osu_with(
                |osu| osu.client_id = Some("1234".into()),
                |candidate| {
                    assert_eq!(candidate.osu.client_id.as_deref(), Some("1234"));
                    Err("settings denied".into())
                },
            )
            .unwrap_err();

        assert_eq!(error, "settings denied");
        assert!(state.settings().osu.client_id.is_none());
    }

    #[test]
    fn settings_transaction_error_preserves_primary_and_rollback_failures() {
        assert_eq!(
            settings_transaction_error("save failed".into(), Vec::new()),
            "save failed"
        );
        let error = settings_transaction_error(
            "save failed".into(),
            vec![
                "restore autostart failed".into(),
                "restore hotkey failed".into(),
            ],
        );
        assert!(error.starts_with("save failed"), "{error}");
        assert!(error.contains("restore autostart failed"), "{error}");
        assert!(error.contains("restore hotkey failed"), "{error}");
    }

    #[test]
    fn sync_global_hotkeys_skips_unregister_when_old_shortcut_is_stale() {
        let old_shortcut = parse_hotkey("Alt+F10").unwrap();
        let new_shortcut = parse_hotkey("Ctrl+F8").unwrap();
        let mut registered = Vec::new();
        let mut unregistered = Vec::new();

        let result = sync_global_hotkeys(
            &[old_shortcut],
            &[new_shortcut],
            |_| false,
            |shortcut| {
                registered.push(shortcut);
                Ok::<_, &'static str>(())
            },
            |shortcut| {
                unregistered.push(shortcut);
                Err::<(), _>("old shortcut was never registered")
            },
        );

        assert_eq!(result, Ok(Vec::new()));
        assert_eq!(registered, vec![new_shortcut]);
        assert!(unregistered.is_empty());
    }

    #[test]
    fn missing_unchanged_global_hotkey_is_retried_without_blocking_save() {
        let shortcut = parse_hotkey("Alt+F10").unwrap();
        let mut registered = Vec::new();

        let result = sync_global_hotkeys(
            &[shortcut],
            &[shortcut],
            |_| false,
            |shortcut| {
                registered.push(shortcut);
                Err::<(), _>("still owned by another app")
            },
            |_| Ok(()),
        );

        let warnings = result.expect("retry failure must not block save");
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("still owned by another app"));
        assert_eq!(registered, vec![shortcut]);
    }

    #[test]
    fn sync_global_hotkeys_adds_secondary_and_keeps_registered_primary() {
        let primary = parse_hotkey("Alt+F10").unwrap();
        let secondary = parse_hotkey("Ctrl+F8").unwrap();
        let mut registered = Vec::new();
        let mut unregistered = Vec::new();

        let result = sync_global_hotkeys(
            &[primary],
            &[primary, secondary],
            |shortcut| shortcut == primary,
            |shortcut| {
                registered.push(shortcut);
                Ok::<_, &'static str>(())
            },
            |shortcut| {
                unregistered.push(shortcut);
                Ok(())
            },
        );

        assert_eq!(result, Ok(Vec::new()));
        assert_eq!(registered, vec![secondary]);
        assert!(unregistered.is_empty());
    }

    #[test]
    fn sync_global_hotkeys_rolls_back_new_registrations_on_failure() {
        let secondary = parse_hotkey("Ctrl+F8").unwrap();
        let removed = parse_hotkey("Alt+F10").unwrap();
        let mut unregistered = Vec::new();

        let result = sync_global_hotkeys(
            &[removed],
            &[secondary],
            |shortcut| shortcut == removed,
            |_| Ok::<_, &'static str>(()),
            |shortcut| {
                unregistered.push(shortcut);
                if shortcut == removed {
                    Err("cannot unregister")
                } else {
                    Ok(())
                }
            },
        );

        assert!(result.is_err());
        assert_eq!(unregistered, vec![removed, secondary]);
    }

    #[test]
    fn sync_global_hotkeys_restores_earlier_removals_when_a_later_one_fails() {
        let first = parse_hotkey("Alt+F10").unwrap();
        let second = parse_hotkey("Ctrl+F8").unwrap();
        let mut registered = Vec::new();
        let mut unregistered = Vec::new();

        let result = sync_global_hotkeys(
            &[first, second],
            &[],
            |_| true,
            |shortcut| {
                registered.push(shortcut);
                Ok::<_, &'static str>(())
            },
            |shortcut| {
                unregistered.push(shortcut);
                if shortcut == second {
                    Err("second removal failed")
                } else {
                    Ok(())
                }
            },
        );

        assert!(result.is_err());
        assert_eq!(unregistered, vec![first, second]);
        assert_eq!(registered, vec![first]);
    }

    #[test]
    fn sync_global_hotkeys_surfaces_rollback_failures() {
        let first = parse_hotkey("Alt+F10").unwrap();
        let second = parse_hotkey("Ctrl+F8").unwrap();

        let error = sync_global_hotkeys(
            &[first, second],
            &[],
            |_| true,
            |_| Err::<(), _>("restore failed"),
            |shortcut| {
                if shortcut == second {
                    Err("second removal failed")
                } else {
                    Ok(())
                }
            },
        )
        .unwrap_err();

        assert!(error.contains("rollback incomplete"), "{error}");
        assert!(error.contains("restore failed"), "{error}");
    }

    #[test]
    fn sync_global_hotkeys_removes_dropped_secondary() {
        let primary = parse_hotkey("Alt+F10").unwrap();
        let secondary = parse_hotkey("Ctrl+F8").unwrap();
        let mut registered = Vec::new();
        let mut unregistered = Vec::new();

        let result = sync_global_hotkeys(
            &[primary, secondary],
            &[primary],
            |_| true,
            |shortcut| {
                registered.push(shortcut);
                Ok::<_, &'static str>(())
            },
            |shortcut| {
                unregistered.push(shortcut);
                Ok(())
            },
        );

        assert_eq!(result, Ok(Vec::new()));
        assert!(registered.is_empty());
        assert_eq!(unregistered, vec![secondary]);
    }

    #[test]
    fn request_save_debounces_only_immediate_duplicate_triggers() {
        let (tx, rx) = mpsc::channel();
        let state = RuntimeState::with_sender(tx, AppSettings::default(), None);

        assert!(state.request_save());
        assert!(matches!(rx.try_recv(), Ok(Cmd::Save)));

        assert!(!state.request_save());
        assert!(rx.try_recv().is_err());

        {
            let mut inner = state.0.lock().unwrap();
            inner.last_save_request = Some(Instant::now() - Duration::from_millis(151));
        }

        assert!(state.request_save());
        assert!(matches!(rx.try_recv(), Ok(Cmd::Save)));
    }

    #[test]
    fn quota_lock_blocks_save_commands_and_preserves_recording_intent_after_stop() {
        let (tx, rx) = mpsc::channel();
        let state = RuntimeState::with_sender(tx, AppSettings::default(), None);
        let generation = state.0.lock().unwrap().recording_generation;
        let event = Event::StorageQuotaFull {
            total_bytes: 100,
            quota_bytes: 100,
            required_bytes: 10,
        };
        assert!(state.accept_service_quota(generation, &event));

        assert!(!state.request_save());
        assert!(rx.try_recv().is_err());
        assert!(state.accept_service_status(generation, false));
        let inner = state.0.lock().unwrap();
        assert!(inner.tx.is_none());
        assert!(inner.recording_desired);
        assert!(inner.quota_blocked.is_some());
    }

    #[test]
    fn quota_lock_prevents_prepared_recorder_restarts() {
        let mut inner = RuntimeState::new(AppSettings::default(), None)
            .0
            .into_inner()
            .unwrap();
        inner.recording_desired = true;
        inner.quota_blocked = Some(Event::StorageQuotaFull {
            total_bytes: 100,
            quota_bytes: 100,
            required_bytes: 1,
        });

        let prepared = RuntimeState::prepare_service_restart(&mut inner).unwrap();

        assert!(prepared.replacement.is_none());
        assert!(!prepared.waiting_for_game);
    }

    #[test]
    fn stale_recorder_cannot_quota_lock_a_new_generation() {
        let (old_tx, _old_rx) = mpsc::channel();
        let state = RuntimeState::with_sender(old_tx, AppSettings::default(), None);
        let old_generation = state.0.lock().unwrap().recording_generation;
        let (new_tx, _new_rx) = mpsc::channel();
        {
            let mut inner = state.0.lock().unwrap();
            RuntimeState::install_recording_sender(&mut inner, new_tx);
        }
        let event = Event::StorageQuotaFull {
            total_bytes: 100,
            quota_bytes: 100,
            required_bytes: 1,
        };

        assert!(!state.accept_service_quota(old_generation, &event));
        assert!(state.0.lock().unwrap().quota_blocked.is_none());
    }

    #[test]
    fn stopped_status_clears_matching_recording_sender() {
        let (tx, rx) = mpsc::channel();
        let state = RuntimeState::with_sender(tx, AppSettings::default(), None);
        let generation = {
            let mut inner = state.0.lock().unwrap();
            inner.last_save_request = Some(Instant::now());
            inner.recording_generation
        };

        assert!(state.accept_service_status(generation, false));

        let inner = state.0.lock().unwrap();
        assert!(inner.tx.is_none());
        assert!(inner.last_save_request.is_none());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn stale_stopped_status_does_not_clear_newer_recording_sender() {
        let (old_tx, _old_rx) = mpsc::channel();
        let state = RuntimeState::with_sender(old_tx, AppSettings::default(), None);
        let stale_generation = state.0.lock().unwrap().recording_generation;
        let (new_tx, new_rx) = mpsc::channel();
        {
            let mut inner = state.0.lock().unwrap();
            RuntimeState::install_recording_sender(&mut inner, new_tx);
        }

        assert!(!state.accept_service_status(stale_generation, false));
        assert!(state.send(Cmd::Save));
        assert!(matches!(new_rx.try_recv(), Ok(Cmd::Save)));
    }

    #[test]
    fn stale_stopped_status_is_rejected_after_entering_waiting() {
        let (tx, _rx) = mpsc::channel();
        let mut settings = AppSettings::default();
        settings.games.pause_when_no_game = true;
        let state = RuntimeState::with_sender(tx, settings, None);
        let stale_generation = state.0.lock().unwrap().recording_generation;

        let waiting_generation = {
            let mut inner = state.0.lock().unwrap();
            RuntimeState::prepare_service_restart(&mut inner)
                .unwrap()
                .waiting_generation
                .unwrap()
        };

        assert!(!state.accept_service_status(stale_generation, false));
        assert!(state.waiting_generation_is_current(waiting_generation));
    }

    #[test]
    fn armed_waiting_status_is_available_for_frontend_replay() {
        let mut settings = AppSettings::default();
        settings.games.pause_when_no_game = true;
        let state = RuntimeState::new(settings, None);
        {
            let mut inner = state.0.lock().unwrap();
            inner.recording_desired = true;
        }

        assert!(matches!(
            state.current_waiting_status(),
            Some(Event::Status {
                recording: false,
                waiting_for_game: true,
                ..
            })
        ));
    }

    #[test]
    fn prepared_settings_restart_is_non_mutating_until_commit() {
        let (tx, rx) = mpsc::channel();
        let original = AppSettings::default();
        let state = RuntimeState::with_sender(tx, original.clone(), None);
        let mut changed = original.clone();
        changed.fps = 120;

        let prepared = state.prepare_settings_restart(changed).unwrap();

        assert_eq!(state.settings().fps, original.fps);
        assert!(state.send(Cmd::Save), "active sender must remain installed");
        assert!(matches!(rx.try_recv(), Ok(Cmd::Save)));
        assert_eq!(prepared.settings.fps, 120);

        drop(prepared); // Simulates a later tray-label or hook-registration failure.
        assert!(
            state.send(Cmd::Save),
            "dropping a plan must not stop recording"
        );
        assert!(matches!(rx.try_recv(), Ok(Cmd::Save)));
    }

    #[test]
    fn settings_save_lock_remains_held_through_runtime_commit() {
        let save_lock = Mutex::new(());
        let save_guard = save_lock.lock().unwrap();
        let original = AppSettings::default();
        let state = RuntimeState::new(original.clone(), None);
        let changed = AppSettings {
            fps: 120,
            ..original
        };
        let prepared = state.prepare_settings_restart(changed).unwrap();

        run_before_releasing_settings_save_lock(save_guard, || {
            let committed: CommittedRuntimeRestart<()> = {
                let mut inner = state.0.lock().unwrap();
                RuntimeState::commit_prepared_restart_with(&mut inner, prepared, |_| {
                    unreachable!("inactive runtime must not spawn a replacement")
                })
                .unwrap()
            };

            assert!(committed.old_tx.is_none());
            assert_eq!(state.settings().fps, 120);
            assert!(
                matches!(
                    save_lock.try_lock(),
                    Err(std::sync::TryLockError::WouldBlock)
                ),
                "settings save lock was released before the runtime commit completed"
            );
            Ok(())
        })
        .unwrap();

        assert!(save_lock.try_lock().is_ok());
    }

    fn detected_game(id: &str, name: &str, hwnd: isize) -> DetectedGame {
        DetectedGame {
            identity: crate::game_identity::GameIdentity::custom(id),
            name: name.into(),
            hwnd,
            window_title: format!("{name} Window"),
            process_id: hwnd as u32,
            exe_name: format!("{name}.exe"),
            exe_path: None,
            recording_mode: GameRecordingMode::FullSession,
        }
    }

    fn detected_built_in_game(id: &str, name: &str, hwnd: isize) -> DetectedGame {
        DetectedGame {
            identity: crate::game_identity::GameIdentity::built_in_plugin(id)
                .expect("test built-in id"),
            name: name.into(),
            hwnd,
            window_title: format!("{name} Window"),
            process_id: hwnd as u32,
            exe_name: format!("{name}.exe"),
            exe_path: None,
            recording_mode: GameRecordingMode::FullSession,
        }
    }

    #[test]
    fn elevated_game_warning_requires_lower_privilege_clipline() {
        let game = detected_game("endfield", "Arknights: Endfield", 42);

        let blocked = GameDetectionEvent::from_detected_with_elevation(
            Some(&game),
            Ok(false),
            |process_id| Ok(process_id == 42),
        );
        assert!(blocked.elevated_hotkeys_blocked);

        let already_elevated =
            GameDetectionEvent::from_detected_with_elevation(Some(&game), Ok(true), |_| Ok(true));
        assert!(!already_elevated.elevated_hotkeys_blocked);

        let ordinary_game =
            GameDetectionEvent::from_detected_with_elevation(Some(&game), Ok(false), |_| Ok(false));
        assert!(!ordinary_game.elevated_hotkeys_blocked);

        let inactive =
            GameDetectionEvent::from_detected_with_elevation(None, Ok(false), |_| Ok(true));
        assert!(!inactive.elevated_hotkeys_blocked);
    }

    #[test]
    fn elevated_game_warning_carries_process_instance_identity() {
        let game = detected_game("endfield", "Arknights: Endfield", 42);

        let event = GameDetectionEvent::from_detected_with_process_queries(
            Some(&game),
            Ok(false),
            |_| Ok(true),
            |process_id| Ok(format!("{process_id}:987654321")),
        );

        assert_eq!(event.process_instance_id.as_deref(), Some("42:987654321"));
    }

    #[test]
    fn elevated_game_warning_is_conservative_when_elevation_cannot_be_queried() {
        let game = detected_game("endfield", "Arknights: Endfield", 42);

        let blocked =
            GameDetectionEvent::from_detected_with_elevation(Some(&game), Ok(false), |_| {
                Err("protected process".to_string())
            });
        assert!(blocked.elevated_hotkeys_blocked);

        let unknown_clipline = GameDetectionEvent::from_detected_with_elevation(
            Some(&game),
            Err("token query failed".to_string()),
            |_| Ok(true),
        );
        assert!(!unknown_clipline.elevated_hotkeys_blocked);
    }

    #[test]
    fn games_only_policy_requires_detection_and_an_active_game() {
        let mut settings = AppSettings::default();
        settings.games.pause_when_no_game = true;

        assert!(!recorder_should_run(&settings, None));
        assert!(recorder_should_run(
            &settings,
            Some(&detected_game("custom-game", "Game", 42))
        ));

        settings.games.auto_detect = false;
        assert!(recorder_should_run(&settings, None));

        settings.games.auto_detect = true;
        settings.games.pause_when_no_game = false;
        assert!(recorder_should_run(&settings, None));
    }

    #[test]
    fn manual_session_bypasses_games_only_waiting_with_full_session_mode() {
        let mut settings = AppSettings::default();
        settings.games.pause_when_no_game = true;
        let state = RuntimeState::new(settings, None);
        let mut inner = state.0.lock().unwrap();
        inner.recording_desired = true;
        inner.manual_full_session_desired = true;

        let prepared = RuntimeState::prepare_service_restart(&mut inner).unwrap();
        let (options, _) = prepared.replacement.expect("manual recording starts capture");

        assert_eq!(options.recording_mode, service::RecordingMode::FullSession);
        assert!(!prepared.waiting_for_game);
    }

    #[test]
    fn quota_blocked_manual_session_request_does_not_arm_a_future_recording() {
        let state = RuntimeState::new(AppSettings::default(), None);
        let mut inner = state.0.lock().unwrap();
        inner.quota_blocked = Some(Event::StorageQuotaFull {
            total_bytes: 10,
            quota_bytes: 10,
            required_bytes: 1,
        });

        let blocked = RuntimeState::arm_manual_session_unless_blocked(&mut inner);

        assert!(blocked.is_some());
        assert!(!inner.manual_full_session_desired);
        assert!(!inner.recording_desired);
    }

    #[test]
    fn stopping_manual_session_returns_games_only_capture_to_waiting() {
        let (tx, _rx) = mpsc::channel();
        let mut settings = AppSettings::default();
        settings.games.pause_when_no_game = true;
        let mut inner = RuntimeInner {
            tx: Some(tx),
            recording_generation: 1,
            recording_desired: true,
            manual_full_session_desired: true,
            settings,
            lol_url: None,
            active_game: None,
            osu_title_events: Vec::new(),
            last_save_request: None,
            decodable_codecs: vec![service::Codec::H264],
            last_recorder_status: None,
            last_storage_status: None,
            recent_recorder_error: false,
            quota_blocked: None,
        };

        let (_, restart) = RuntimeState::prepare_manual_session_stop(&mut inner).unwrap();
        let restart = restart.expect("games-only capture must return to waiting");

        assert!(!inner.manual_full_session_desired);
        assert!(inner.recording_desired);
        assert!(restart.old_tx.is_some());
        assert!(restart.replacement.is_none());
        assert!(restart.waiting_for_game);
    }

    #[test]
    fn game_restart_pauses_service_but_keeps_recorder_armed() {
        let (tx, _rx) = mpsc::channel();
        let mut settings = AppSettings::default();
        settings.games.pause_when_no_game = true;
        let mut inner = RuntimeInner {
            tx: Some(tx),
            recording_generation: 1,
            recording_desired: true,
            manual_full_session_desired: false,
            settings,
            lol_url: None,
            active_game: None,
            osu_title_events: Vec::new(),
            last_save_request: Some(Instant::now()),
            decodable_codecs: vec![service::Codec::H264],
            last_recorder_status: None,
            last_storage_status: None,
            recent_recorder_error: false,
            quota_blocked: None,
        };

        let prepared = RuntimeState::prepare_service_restart(&mut inner).unwrap();

        assert!(prepared.old_tx.is_some());
        assert!(prepared.replacement.is_none());
        assert!(prepared.waiting_for_game);
        assert!(inner.recording_desired);
        assert!(inner.tx.is_none());
    }

    #[test]
    fn game_restart_resumes_an_armed_policy_pause() {
        let mut settings = AppSettings::default();
        settings.games.pause_when_no_game = true;
        let mut inner = RuntimeInner {
            tx: None,
            recording_generation: 4,
            recording_desired: true,
            manual_full_session_desired: false,
            settings,
            lol_url: None,
            active_game: Some(detected_game("custom-game", "Game", 42)),
            osu_title_events: Vec::new(),
            last_save_request: None,
            decodable_codecs: vec![service::Codec::H264],
            last_recorder_status: None,
            last_storage_status: None,
            recent_recorder_error: false,
            quota_blocked: None,
        };

        let prepared = RuntimeState::prepare_service_restart(&mut inner).unwrap();

        assert!(prepared.old_tx.is_none());
        assert!(prepared.replacement.is_some());
        assert!(!prepared.waiting_for_game);
        assert!(inner.recording_desired);
    }

    #[test]
    fn manual_stop_invalidates_a_pending_waiting_status() {
        let mut settings = AppSettings::default();
        settings.games.pause_when_no_game = true;
        let state = RuntimeState::new(settings, None);
        let waiting_generation = {
            let mut inner = state.0.lock().unwrap();
            inner.recording_desired = true;
            RuntimeState::prepare_service_restart(&mut inner)
                .unwrap()
                .waiting_generation
                .expect("policy pause must carry its generation")
        };

        state.stop_recording().unwrap();

        assert!(!state.waiting_generation_is_current(waiting_generation));
    }

    #[test]
    fn enabling_games_only_policy_stops_fallback_capture_at_commit() {
        let (tx, _rx) = mpsc::channel();
        let state = RuntimeState::with_sender(tx, AppSettings::default(), None);
        let mut changed = AppSettings::default();
        changed.games.pause_when_no_game = true;
        let prepared = state.prepare_settings_restart(changed).unwrap();

        let mut spawned = false;
        let committed = {
            let mut inner = state.0.lock().unwrap();
            RuntimeState::commit_prepared_restart_with(&mut inner, prepared, |_| {
                spawned = true;
                let (replacement_tx, _replacement_rx) = mpsc::channel();
                (replacement_tx, ())
            })
            .unwrap()
        };

        assert!(!spawned);
        assert!(committed.old_tx.is_some());
        assert!(committed.replacement.is_none());
        assert!(committed.waiting_for_game);
        assert!(state.0.lock().unwrap().recording_desired);
    }

    #[test]
    fn committed_waiting_invalidates_detector_restart_already_spawning() {
        let (initial_tx, _initial_rx) = mpsc::channel();
        let state = RuntimeState::with_sender(initial_tx, AppSettings::default(), None);
        let detector_restart = {
            let mut inner = state.0.lock().unwrap();
            RuntimeState::prepare_service_restart(&mut inner).unwrap()
        };
        let (_options, detector_generation) = detector_restart.replacement.unwrap();

        let mut waiting_settings = AppSettings::default();
        waiting_settings.games.pause_when_no_game = true;
        let prepared = state.prepare_settings_restart(waiting_settings).unwrap();
        let committed: CommittedRuntimeRestart<()> = {
            let mut inner = state.0.lock().unwrap();
            RuntimeState::commit_prepared_restart_with(&mut inner, prepared, |_| {
                unreachable!("waiting must not spawn a recorder")
            })
            .unwrap()
        };
        assert!(committed.waiting_for_game);
        assert!(committed.old_tx.is_none());

        let (stale_tx, stale_rx) = mpsc::channel();
        let rejected = {
            let mut inner = state.0.lock().unwrap();
            RuntimeState::install_prepared_service_restart(
                &mut inner,
                detector_generation,
                stale_tx,
            )
            .unwrap_err()
        };
        rejected.send(Cmd::Stop { announce: false }).unwrap();

        assert!(matches!(
            stale_rx.try_recv(),
            Ok(Cmd::Stop { announce: false })
        ));
        assert!(!state.send(Cmd::Save));
    }

    #[test]
    fn prepared_settings_restart_uses_current_game_and_sender_at_commit() {
        let (initial_tx, _initial_rx) = mpsc::channel();
        let state = RuntimeState::with_sender(initial_tx, AppSettings::default(), None);
        {
            state.0.lock().unwrap().active_game = Some(detected_built_in_game(
                crate::game_plugins::LEAGUE_OF_LEGENDS_ID,
                "League",
                41,
            ));
        }
        let changed = AppSettings {
            fps: 120,
            ..AppSettings::default()
        };
        let prepared = state.prepare_settings_restart(changed).unwrap();

        let (newer_tx, newer_rx) = mpsc::channel();
        let (replacement_tx, replacement_rx) = mpsc::channel();
        let mut committed_options = None;
        let committed = {
            let mut inner = state.0.lock().unwrap();
            inner.active_game = Some(detected_built_in_game(
                crate::game_plugins::OSU_ID,
                "osu!",
                84,
            ));
            RuntimeState::install_recording_sender(&mut inner, newer_tx);
            RuntimeState::commit_prepared_restart_with(&mut inner, prepared, |options| {
                committed_options = Some(options);
                (replacement_tx, ())
            })
            .unwrap()
        };

        let options = committed_options.unwrap();
        assert_eq!(options.fps, 120);
        assert_eq!(
            options.capture_source,
            service::CaptureSource::WindowHandle {
                hwnd: 84,
                title: "osu! Window".into(),
            }
        );
        assert_eq!(
            options.active_game.as_ref().map(|game| game.identity.id()),
            Some(crate::game_plugins::OSU_ID)
        );
        committed.old_tx.unwrap().send(Cmd::Save).unwrap();
        assert!(matches!(newer_rx.try_recv(), Ok(Cmd::Save)));
        assert!(committed.replacement.is_some());
        assert!(state.send(Cmd::Save));
        assert!(matches!(replacement_rx.try_recv(), Ok(Cmd::Save)));
    }

    #[test]
    fn prepared_settings_restart_restarts_sender_that_started_before_commit() {
        let state = RuntimeState::new(AppSettings::default(), None);
        let changed = AppSettings {
            fps: 120,
            ..AppSettings::default()
        };
        let prepared = state.prepare_settings_restart(changed).unwrap();

        let (started_tx, started_rx) = mpsc::channel();
        let (replacement_tx, replacement_rx) = mpsc::channel();
        let mut committed_options = None;
        let committed = {
            let mut inner = state.0.lock().unwrap();
            RuntimeState::install_recording_sender(&mut inner, started_tx);
            RuntimeState::commit_prepared_restart_with(&mut inner, prepared, |options| {
                committed_options = Some(options);
                (replacement_tx, ())
            })
            .unwrap()
        };

        assert_eq!(committed_options.unwrap().fps, 120);
        committed.old_tx.unwrap().send(Cmd::Save).unwrap();
        assert!(matches!(started_rx.try_recv(), Ok(Cmd::Save)));
        assert!(committed.replacement.is_some());
        assert!(state.send(Cmd::Save));
        assert!(matches!(replacement_rx.try_recv(), Ok(Cmd::Save)));
    }

    #[test]
    fn prepared_settings_restart_does_not_resurrect_sender_stopped_before_commit() {
        let (tx, _rx) = mpsc::channel();
        let state = RuntimeState::with_sender(tx, AppSettings::default(), None);
        let changed = AppSettings {
            fps: 120,
            ..AppSettings::default()
        };
        let prepared = state.prepare_settings_restart(changed).unwrap();

        state.stop_recording().unwrap();

        let mut spawned = false;
        let committed = {
            let mut inner = state.0.lock().unwrap();
            RuntimeState::commit_prepared_restart_with(&mut inner, prepared, |_| {
                spawned = true;
                let (replacement_tx, _replacement_rx) = mpsc::channel();
                (replacement_tx, ())
            })
            .unwrap()
        };

        assert!(!spawned);
        assert!(committed.old_tx.is_none());
        assert!(committed.replacement.is_none());
        assert!(!state.send(Cmd::Save));
        assert_eq!(state.settings().fps, 120);
    }

    #[test]
    fn commit_time_restart_option_error_keeps_current_sender_and_settings() {
        let (tx, rx) = mpsc::channel();
        let original = AppSettings::default();
        let state = RuntimeState::with_sender(tx, original.clone(), None);
        let prepared = PreparedRuntimeRestart {
            settings: invalid_disk_replay_settings(),
        };

        let mut spawned = false;
        let error = {
            let mut inner = state.0.lock().unwrap();
            RuntimeState::commit_prepared_restart_with(&mut inner, prepared, |_| {
                spawned = true;
                let (replacement_tx, _replacement_rx) = mpsc::channel();
                (replacement_tx, ())
            })
            .unwrap_err()
        };

        assert!(error.contains("replay cache folder"), "{error}");
        assert!(!spawned);
        assert_eq!(state.settings().replay_storage, original.replay_storage);
        assert!(state.send(Cmd::Save));
        assert!(matches!(rx.try_recv(), Ok(Cmd::Save)));
    }

    #[test]
    fn recording_sender_survives_restart_option_error() {
        let (tx, _rx) = mpsc::channel();
        let mut inner = RuntimeInner {
            tx: Some(tx),
            recording_generation: 1,
            recording_desired: true,
            manual_full_session_desired: false,
            settings: invalid_disk_replay_settings(),
            lol_url: None,
            active_game: None,
            osu_title_events: Vec::new(),
            last_save_request: Some(Instant::now()),
            decodable_codecs: vec![service::Codec::H264],
            last_recorder_status: None,
            last_storage_status: None,
            recent_recorder_error: false,
            quota_blocked: None,
        };

        let err = match RuntimeState::prepare_service_restart(&mut inner) {
            Ok(_) => panic!("restart options should fail"),
            Err(err) => err,
        };

        assert!(err.contains("replay cache folder"), "{err}");
        assert!(inner.tx.is_some(), "failed options must not drop sender");
        assert!(inner.recording_desired);
        assert_eq!(inner.recording_generation, 1);
        assert!(
            inner.last_save_request.is_some(),
            "failed options must not clear debounce state"
        );
    }

    #[test]
    fn prepared_restart_skips_abandoned_recording_recovery() {
        let (tx, _rx) = mpsc::channel();
        let mut inner = RuntimeInner {
            tx: Some(tx),
            recording_generation: 1,
            recording_desired: true,
            manual_full_session_desired: false,
            settings: AppSettings::default(),
            lol_url: None,
            active_game: None,
            osu_title_events: Vec::new(),
            last_save_request: Some(Instant::now()),
            decodable_codecs: vec![service::Codec::H264],
            last_recorder_status: None,
            last_storage_status: None,
            recent_recorder_error: false,
            quota_blocked: None,
        };

        let prepared = RuntimeState::prepare_service_restart(&mut inner).unwrap();
        let (next_options, _generation) = prepared.replacement.unwrap();

        assert!(
            !next_options.recover_abandoned_recordings,
            "internal recorder restarts must not recover another active recorder's temp file"
        );
    }

    #[test]
    fn game_restart_gap_does_not_resurrect_after_user_stop() {
        let (initial_tx, _initial_rx) = mpsc::channel();
        let state = RuntimeState::with_sender(initial_tx, AppSettings::default(), None);
        let prepared = {
            let mut inner = state.0.lock().unwrap();
            RuntimeState::prepare_service_restart(&mut inner).unwrap()
        };
        let (_options, restart_generation) = prepared.replacement.unwrap();

        state.stop_recording().unwrap();
        let (replacement_tx, replacement_rx) = mpsc::channel();
        let rejected = {
            let mut inner = state.0.lock().unwrap();
            RuntimeState::install_prepared_service_restart(
                &mut inner,
                restart_generation,
                replacement_tx,
            )
            .unwrap_err()
        };
        rejected.send(Cmd::Stop { announce: false }).unwrap();

        assert!(
            !state.send(Cmd::Save),
            "a replacement spawned before Stop must not resurrect recording"
        );
        assert!(matches!(
            replacement_rx.try_recv(),
            Ok(Cmd::Stop { announce: false })
        ));
    }

    #[test]
    fn game_restart_gap_does_not_overwrite_a_newer_manual_start() {
        let (initial_tx, _initial_rx) = mpsc::channel();
        let state = RuntimeState::with_sender(initial_tx, AppSettings::default(), None);
        let prepared = {
            let mut inner = state.0.lock().unwrap();
            RuntimeState::prepare_service_restart(&mut inner).unwrap()
        };
        let (_options, restart_generation) = prepared.replacement.unwrap();

        let (newer_tx, newer_rx) = mpsc::channel();
        let (stale_tx, stale_rx) = mpsc::channel();
        let rejected = {
            let mut inner = state.0.lock().unwrap();
            RuntimeState::install_recording_sender(&mut inner, newer_tx);
            RuntimeState::install_prepared_service_restart(&mut inner, restart_generation, stale_tx)
                .unwrap_err()
        };
        rejected.send(Cmd::Stop { announce: false }).unwrap();

        assert!(state.send(Cmd::Save));
        assert!(
            matches!(newer_rx.try_recv(), Ok(Cmd::Save)),
            "the manual start must remain the active sender"
        );
        assert!(matches!(
            stale_rx.try_recv(),
            Ok(Cmd::Stop { announce: false })
        ));
    }

    #[test]
    fn newer_game_restart_supersedes_a_restart_already_spawning() {
        let (initial_tx, _initial_rx) = mpsc::channel();
        let state = RuntimeState::with_sender(initial_tx, AppSettings::default(), None);
        let first = {
            let mut inner = state.0.lock().unwrap();
            RuntimeState::prepare_service_restart(&mut inner).unwrap()
        };
        let (_first_options, first_generation) = first.replacement.unwrap();

        let second = {
            let mut inner = state.0.lock().unwrap();
            RuntimeState::prepare_service_restart(&mut inner).unwrap()
        };
        let (_second_options, second_generation) = second.replacement.unwrap();

        let (first_tx, first_rx) = mpsc::channel();
        let (second_tx, second_rx) = mpsc::channel();
        let first_rejected = {
            let mut inner = state.0.lock().unwrap();
            let rejected = RuntimeState::install_prepared_service_restart(
                &mut inner,
                first_generation,
                first_tx,
            )
            .unwrap_err();
            RuntimeState::install_prepared_service_restart(
                &mut inner,
                second_generation,
                second_tx,
            )
            .unwrap();
            rejected
        };
        first_rejected.send(Cmd::Stop { announce: false }).unwrap();
        assert!(matches!(
            first_rx.try_recv(),
            Ok(Cmd::Stop { announce: false })
        ));
        assert!(state.send(Cmd::Save));
        assert!(matches!(second_rx.try_recv(), Ok(Cmd::Save)));
    }

    #[test]
    fn recording_sender_survives_game_restart_option_error() {
        let (tx, _rx) = mpsc::channel();
        let mut inner = RuntimeInner {
            tx: Some(tx),
            recording_generation: 1,
            recording_desired: true,
            manual_full_session_desired: false,
            settings: invalid_disk_replay_settings(),
            lol_url: None,
            active_game: Some(DetectedGame {
                identity: crate::game_identity::GameIdentity::custom("custom-game"),
                name: "Game".into(),
                hwnd: 42,
                window_title: "Game".into(),
                process_id: 7,
                exe_name: "game.exe".into(),
                exe_path: None,
                recording_mode: GameRecordingMode::FullSession,
            }),
            osu_title_events: Vec::new(),
            last_save_request: Some(Instant::now()),
            decodable_codecs: vec![service::Codec::H264],
            last_recorder_status: None,
            last_storage_status: None,
            recent_recorder_error: false,
            quota_blocked: None,
        };

        let err = match RuntimeState::prepare_service_restart(&mut inner) {
            Ok(_) => panic!("restart options should fail"),
            Err(err) => err,
        };

        assert!(err.contains("replay cache folder"), "{err}");
        assert!(inner.tx.is_some(), "failed options must not drop sender");
        assert!(
            inner.last_save_request.is_some(),
            "failed options must not clear debounce state"
        );
    }

    #[test]
    fn failed_newer_game_restart_invalidates_a_plan_already_spawning() {
        let mut inner = RuntimeInner {
            tx: None,
            recording_generation: 7,
            recording_desired: true,
            manual_full_session_desired: false,
            settings: invalid_disk_replay_settings(),
            lol_url: None,
            active_game: None,
            osu_title_events: Vec::new(),
            last_save_request: None,
            decodable_codecs: vec![service::Codec::H264],
            last_recorder_status: None,
            last_storage_status: None,
            recent_recorder_error: false,
            quota_blocked: None,
        };

        assert!(RuntimeState::prepare_service_restart(&mut inner).is_err());
        assert_eq!(inner.recording_generation, 8);
        assert!(inner.recording_desired);
        assert!(inner.tx.is_none());
    }

    #[test]
    fn preserve_backend_owned_settings_fields_keeps_upload_state_but_allows_preferences() {
        let mut frontend = AppSettings::default();
        frontend.cloud.host_url = "https://stale.example.com".into();
        frontend.cloud.public_url = Some("https://stale-public.example.com".into());
        frontend.cloud.connected_user_id = Some("stale-user".into());
        frontend.cloud.connected_username = Some("stale-name".into());
        frontend.cloud.connected_display_name = Some("Stale".into());
        frontend.cloud.credential_target = Some("stale-target".into());
        frontend.cloud.default_visibility = "public".into();
        frontend.cloud.delete_local_after_upload = true;
        frontend.cloud.auto_upload_rules = true;

        let mut backend = AppSettings::default();
        backend.cloud.host_url = "https://cloud.example.com".into();
        backend.cloud.public_url = Some("https://public.example.com".into());
        backend.cloud.connected_user_id = Some("user-1".into());
        backend.cloud.connected_username = Some("dain".into());
        backend.cloud.connected_display_name = Some("Dain".into());
        backend.cloud.credential_target = Some("clipline:user-1".into());
        backend.cloud.credential_cleanup_targets = vec!["clipline:old-user".into()];
        backend.cloud.uploads.insert(
            "local-1".into(),
            CloudUploadRecord {
                local_clip_id: "local-1".into(),
                path: "D:\\Videos\\Clipline\\clip.mp4".into(),
                remote_clip_id: Some("remote-1".into()),
                remote_url: Some("https://public.example.com/remote-1".into()),
                visibility: "private".into(),
                upload_status: "uploaded_private".into(),
                error: None,
                updated_at_unix: 42,
            },
        );

        preserve_backend_owned_settings_fields(&mut frontend, &backend);

        assert_eq!(frontend.cloud.host_url, backend.cloud.host_url);
        assert_eq!(frontend.cloud.public_url, backend.cloud.public_url);
        assert_eq!(
            frontend.cloud.connected_user_id,
            backend.cloud.connected_user_id
        );
        assert_eq!(
            frontend.cloud.connected_username,
            backend.cloud.connected_username
        );
        assert_eq!(
            frontend.cloud.connected_display_name,
            backend.cloud.connected_display_name
        );
        assert_eq!(
            frontend.cloud.credential_target,
            backend.cloud.credential_target
        );
        assert_eq!(
            frontend.cloud.credential_cleanup_targets,
            backend.cloud.credential_cleanup_targets
        );
        assert_eq!(frontend.cloud.uploads, backend.cloud.uploads);
        assert_eq!(frontend.cloud.default_visibility, "public");
        assert!(frontend.cloud.delete_local_after_upload);
        assert!(frontend.cloud.auto_upload_rules);
    }

    #[test]
    fn preserve_backend_owned_settings_fields_keeps_osu_credentials_from_backend() {
        let mut frontend = AppSettings::default();
        frontend.osu.client_id = None;
        frontend.osu.user = None;
        frontend.osu.credential_target = None;
        frontend.osu.last_connected_username = None;

        let mut backend = AppSettings::default();
        backend.osu.client_id = Some("61835".into());
        backend.osu.user = Some("3426414".into());
        backend.osu.credential_target = Some("Clipline osu!:61835:3426414".into());
        backend.osu.credential_cleanup_targets = vec!["Clipline osu!:old".into()];
        backend.osu.last_connected_username = Some("Dain".into());

        preserve_backend_owned_settings_fields(&mut frontend, &backend);

        assert_eq!(frontend.osu, backend.osu);
    }

    #[test]
    fn detected_game_identity_ignores_volatile_window_title() {
        let current = DetectedGame {
            identity: crate::game_identity::GameIdentity::custom("custom-game"),
            name: "Game".into(),
            hwnd: 42,
            window_title: "Loading".into(),
            process_id: 7,
            exe_name: "game.exe".into(),
            exe_path: None,
            recording_mode: GameRecordingMode::ReplaysOnly,
        };
        let updated_title = DetectedGame {
            window_title: "Paused".into(),
            ..current.clone()
        };
        let different_window = DetectedGame {
            hwnd: 43,
            ..current.clone()
        };

        assert!(same_game_window(Some(&current), Some(&updated_title)));
        assert!(!same_game_window(Some(&current), Some(&different_window)));
    }

    fn invalid_disk_replay_settings() -> AppSettings {
        AppSettings {
            replay_storage: ReplayStorageSettings {
                mode: ReplayStorageMode::Disk,
                disk_dir: String::new(),
                disk_quota_gb: 2.0,
                disk_acknowledged: true,
            },
            ..AppSettings::default()
        }
    }

    #[test]
    fn detected_game_recording_mode_change_requires_service_restart() {
        let current = DetectedGame {
            identity: crate::game_identity::GameIdentity::custom("custom-game"),
            name: "Game".into(),
            hwnd: 42,
            window_title: "Game".into(),
            process_id: 7,
            exe_name: "game.exe".into(),
            exe_path: None,
            recording_mode: GameRecordingMode::ReplaysOnly,
        };
        let updated_mode = DetectedGame {
            recording_mode: GameRecordingMode::FullSession,
            ..current.clone()
        };
        let updated_title = DetectedGame {
            window_title: "Game - Loading".into(),
            ..current.clone()
        };

        assert!(same_game_window(Some(&current), Some(&updated_mode)));
        assert!(game_recording_mode_changed(
            Some(&current),
            Some(&updated_mode)
        ));
        assert!(!game_recording_mode_changed(
            Some(&current),
            Some(&updated_title)
        ));
    }

    #[test]
    fn osu_title_events_record_only_changed_osu_titles() {
        let mut inner = RuntimeInner {
            tx: None,
            recording_generation: 0,
            recording_desired: false,
            manual_full_session_desired: false,
            settings: AppSettings::default(),
            lol_url: None,
            active_game: None,
            osu_title_events: Vec::new(),
            last_save_request: None,
            decodable_codecs: vec![service::Codec::H264],
            last_recorder_status: None,
            last_storage_status: None,
            recent_recorder_error: false,
            quota_blocked: None,
        };
        let osu = DetectedGame {
            identity: crate::game_identity::GameIdentity::built_in_plugin(
                crate::game_plugins::OSU_ID,
            )
            .unwrap(),
            name: "osu!".into(),
            hwnd: 42,
            window_title: "osu! - xi - Blue Zenith [FOUR DIMENSIONS]".into(),
            process_id: 7,
            exe_name: "osu!.exe".into(),
            exe_path: None,
            recording_mode: GameRecordingMode::FullSession,
        };
        let league = DetectedGame {
            identity: crate::game_identity::GameIdentity::built_in_plugin(
                crate::game_plugins::LEAGUE_OF_LEGENDS_ID,
            )
            .unwrap(),
            name: "League of Legends".into(),
            window_title: "League".into(),
            exe_name: "League of Legends.exe".into(),
            ..osu.clone()
        };

        record_osu_title_event(&mut inner, Some(&osu), 100);
        record_osu_title_event(&mut inner, Some(&osu), 101);
        record_osu_title_event(&mut inner, Some(&league), 102);
        record_osu_title_event(
            &mut inner,
            Some(&DetectedGame {
                identity: crate::game_identity::GameIdentity::custom(crate::game_plugins::OSU_ID),
                name: "Custom impostor".into(),
                window_title: "must not be tracked".into(),
                exe_name: "impostor.exe".into(),
                ..osu.clone()
            }),
            102,
        );
        record_osu_title_event(
            &mut inner,
            Some(&DetectedGame {
                window_title: "osu!".into(),
                ..osu.clone()
            }),
            103,
        );

        assert_eq!(
            inner.osu_title_events,
            vec![
                OsuTitleEvent {
                    unix_s: 100,
                    title: "osu! - xi - Blue Zenith [FOUR DIMENSIONS]".into(),
                },
                OsuTitleEvent {
                    unix_s: 103,
                    title: "osu!".into(),
                }
            ]
        );
    }

    #[test]
    fn osu_title_events_for_window_filters_to_saved_recording_window() {
        let state = RuntimeState::new(AppSettings::default(), None);
        {
            let mut inner = state.0.lock().unwrap();
            inner.osu_title_events = vec![
                OsuTitleEvent {
                    unix_s: 90,
                    title: "too early".into(),
                },
                OsuTitleEvent {
                    unix_s: 96,
                    title: "start margin".into(),
                },
                OsuTitleEvent {
                    unix_s: 150,
                    title: "inside".into(),
                },
                OsuTitleEvent {
                    unix_s: 206,
                    title: "too late".into(),
                },
            ];
        }

        let titles: Vec<_> = state
            .osu_title_events_for_window(Some(100), Some(200))
            .into_iter()
            .map(|event| event.title)
            .collect();

        assert_eq!(titles, vec!["start margin", "inside"]);
    }

    #[test]
    fn built_in_league_profile_counts_as_active_game_configuration() {
        let active = DetectedGame {
            identity: crate::game_identity::GameIdentity::built_in_plugin(
                crate::game_plugins::LEAGUE_OF_LEGENDS_ID,
            )
            .unwrap(),
            name: "League of Legends".into(),
            hwnd: 42,
            window_title: "League of Legends (TM) Client".into(),
            process_id: 7,
            exe_name: "League of Legends.exe".into(),
            exe_path: None,
            recording_mode: GameRecordingMode::FullSession,
        };
        let mut settings = AppSettings::default();

        assert!(active_game_still_configured(&settings, Some(&active)));

        settings.games.plugins.insert(
            crate::game_plugins::LEAGUE_OF_LEGENDS_ID.into(),
            crate::settings::GamePluginSettings {
                enabled: false,
                recording_mode: GameRecordingMode::FullSession,
                review: Default::default(),
            },
        );
        assert!(!active_game_still_configured(&settings, Some(&active)));
    }

    #[test]
    fn window_request_actions_follow_general_settings() {
        let defaults = AppSettings::default();
        assert_eq!(close_request_action(&defaults), CloseRequestAction::Tray);
        assert_eq!(
            minimize_request_action(&defaults),
            MinimizeRequestAction::Taskbar
        );

        let settings = AppSettings {
            close_to_tray: false,
            minimize_to_tray: true,
            ..AppSettings::default()
        };
        assert_eq!(close_request_action(&settings), CloseRequestAction::Quit);
        assert_eq!(
            minimize_request_action(&settings),
            MinimizeRequestAction::Tray
        );
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

    #[test]
    fn native_shell_starts_recorder_after_single_instance_accepts_process() {
        let app = include_str!("app.rs");
        let run_start = app.find("pub fn run()").expect("run function should exist");
        let run_body = &app[run_start..];
        let run_end = run_body
            .find("\nfn spawn_game_detector")
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

    #[test]
    fn webview_repair_notice_is_only_needed_for_dead_webview_signals() {
        assert!(should_show_webview_repair_notice(
            WebviewRepairNoticeReason::GetterFailedToReceiveMessage,
            false,
        ));
        assert!(should_show_webview_repair_notice(
            WebviewRepairNoticeReason::FrontendReadyTimeout,
            false,
        ));
        assert!(!should_show_webview_repair_notice(
            WebviewRepairNoticeReason::OtherGetterError,
            false,
        ));
        assert!(!should_show_webview_repair_notice(
            WebviewRepairNoticeReason::GetterFailedToReceiveMessage,
            true,
        ));
    }

    #[test]
    fn classifies_tauri_runtime_receive_failure_as_dead_webview() {
        let err = tauri::Error::Runtime(tauri_runtime::Error::FailedToReceiveMessage);

        assert_eq!(
            classify_webview_getter_error(&err),
            WebviewRepairNoticeReason::GetterFailedToReceiveMessage
        );
    }

    #[test]
    fn tray_left_click_opens_only_on_button_release() {
        assert!(should_open_on_tray_click(
            MouseButton::Left,
            MouseButtonState::Up
        ));
        assert!(!should_open_on_tray_click(
            MouseButton::Left,
            MouseButtonState::Down
        ));
        assert!(!should_open_on_tray_click(
            MouseButton::Right,
            MouseButtonState::Up
        ));
        assert!(!should_open_on_tray_click(
            MouseButton::Middle,
            MouseButtonState::Up
        ));
    }

    #[test]
    fn app_window_labels_include_only_main_window() {
        assert!(is_app_window_label("main"));
        assert!(!is_app_window_label("main-recovery-1"));
        assert!(!is_app_window_label("settings"));
        assert!(!is_app_window_label("mainframe"));
    }

    #[test]
    fn unresponsive_main_window_reveals_existing_handle() {
        assert_eq!(
            main_window_open_target(true),
            MainWindowOpenTarget::ExistingMain
        );
        assert_eq!(
            main_window_open_target(false),
            MainWindowOpenTarget::NewMain
        );
    }

    #[test]
    fn close_to_tray_destroy_enters_destroying_until_destroyed_event() {
        let mut state = MainWindowShellState::new(WindowLifecycleMode::Foreground, true);

        begin_close_to_tray_destroy(&mut state);
        assert_eq!(state.mode, WindowLifecycleMode::Destroying);
        assert!(
            state.main_window_present,
            "the dying label may still be registered during Destroying"
        );
        assert!(!state.pending_open);

        let action = observe_main_window_destroyed(&mut state);
        assert_eq!(state.mode, WindowLifecycleMode::Destroyed);
        assert!(
            !state.main_window_present,
            "Destroyed must clear the registered main label"
        );
        assert_eq!(action, MainWindowOpenAction::Noop);
    }

    #[test]
    fn open_during_destroying_queues_instead_of_revealing_dying_label() {
        let mut state = MainWindowShellState::new(WindowLifecycleMode::Destroying, true);

        let action = request_main_window_open(&mut state);

        assert_eq!(action, MainWindowOpenAction::QueueOpen);
        assert!(state.pending_open);
        assert_eq!(state.mode, WindowLifecycleMode::Destroying);
        assert!(state.main_window_present);
        assert_ne!(
            main_window_open_target_for(WindowLifecycleMode::Destroying, true),
            MainWindowOpenTarget::ExistingMain
        );
    }

    #[test]
    fn destroyed_with_pending_open_builds_exactly_one_window() {
        let mut state = MainWindowShellState {
            mode: WindowLifecycleMode::Destroying,
            main_window_present: true,
            pending_open: true,
        };

        let action = observe_main_window_destroyed(&mut state);

        assert_eq!(state.mode, WindowLifecycleMode::Destroyed);
        assert!(!state.main_window_present);
        assert!(!state.pending_open);
        assert_eq!(action, MainWindowOpenAction::BuildNew);

        let second = observe_main_window_destroyed(&mut state);
        assert_eq!(second, MainWindowOpenAction::Noop);
        assert!(!state.pending_open);
    }

    #[test]
    fn destroying_or_destroyed_label_is_never_existing_main() {
        assert_ne!(
            main_window_open_target_for(WindowLifecycleMode::Destroying, true),
            MainWindowOpenTarget::ExistingMain
        );
        assert_ne!(
            main_window_open_target_for(WindowLifecycleMode::Destroyed, true),
            MainWindowOpenTarget::ExistingMain
        );
        assert_eq!(
            main_window_open_target_for(WindowLifecycleMode::Destroyed, false),
            MainWindowOpenTarget::NewMain
        );
        assert_eq!(
            main_window_open_target_for(WindowLifecycleMode::Foreground, true),
            MainWindowOpenTarget::ExistingMain
        );
    }

    #[test]
    fn immediate_close_then_open_race_builds_only_after_destroyed() {
        let mut state = MainWindowShellState::new(WindowLifecycleMode::Foreground, true);
        let mut builds = 0;
        let mut reveals = 0;

        begin_close_to_tray_destroy(&mut state);
        assert_eq!(state.mode, WindowLifecycleMode::Destroying);

        match request_main_window_open(&mut state) {
            MainWindowOpenAction::QueueOpen => {}
            MainWindowOpenAction::RevealExisting => reveals += 1,
            MainWindowOpenAction::BuildNew => builds += 1,
            MainWindowOpenAction::Noop => {}
        }
        assert!(
            state.pending_open,
            "open during Destroying must be remembered"
        );
        assert_eq!(reveals, 0, "must not reveal the dying label");
        assert_eq!(builds, 0, "must not build while Destroying");

        match observe_main_window_destroyed(&mut state) {
            MainWindowOpenAction::BuildNew => builds += 1,
            MainWindowOpenAction::RevealExisting => reveals += 1,
            MainWindowOpenAction::QueueOpen | MainWindowOpenAction::Noop => {}
        }

        assert_eq!(state.mode, WindowLifecycleMode::Destroyed);
        assert!(!state.main_window_present);
        assert!(!state.pending_open);
        assert_eq!(reveals, 0);
        assert_eq!(builds, 1);
    }

    #[test]
    fn destroying_and_destroyed_modes_are_backgrounded() {
        assert!(WindowLifecycleSnapshot::new(1, WindowLifecycleMode::Destroying).backgrounded);
        assert!(WindowLifecycleSnapshot::new(2, WindowLifecycleMode::Destroyed).backgrounded);
        assert!(!WindowLifecycleSnapshot::new(3, WindowLifecycleMode::Foreground).backgrounded);
    }

    #[test]
    fn microphone_test_rejects_destroying_and_destroyed_modes() {
        let state = WindowLifecycleState::default();
        state.transition(WindowLifecycleMode::Destroying);
        assert!(ensure_foreground_microphone_test(&state).is_err());
        state.transition(WindowLifecycleMode::Destroyed);
        assert!(ensure_foreground_microphone_test(&state).is_err());
    }

    #[test]
    fn parses_webview2_runtime_version_from_reg_output() {
        let output = r#"
HKEY_CURRENT_USER\Software\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}
    pv    REG_SZ    120.0.2210.55
"#;

        assert_eq!(
            parse_reg_pv_output(output).as_deref(),
            Some("120.0.2210.55")
        );
    }

    #[test]
    fn opening_main_window_restores_before_focus() {
        let calls = std::cell::RefCell::new(Vec::new());

        reveal_main_window(
            || calls.borrow_mut().push("memory_normal"),
            || {
                calls.borrow_mut().push("webview_show");
                Ok::<(), String>(())
            },
            || {
                calls.borrow_mut().push("show");
                Ok::<(), String>(())
            },
            || {
                calls.borrow_mut().push("unminimize");
                Ok::<(), String>(())
            },
            || {
                calls.borrow_mut().push("focus");
                Ok::<(), String>(())
            },
            || calls.borrow_mut().push("foreground"),
        )
        .unwrap();

        // Normal before the webview is shown, and the webview before the native
        // window, or the user sees a throttled or transparent first frame.
        assert_eq!(
            *calls.borrow(),
            [
                "memory_normal",
                "webview_show",
                "show",
                "unminimize",
                "foreground",
                "focus"
            ]
        );
    }

    #[test]
    fn reveal_continues_when_webview_visibility_fails() {
        let calls = std::cell::RefCell::new(Vec::new());

        // Webview visibility is best-effort: failing it must never leave the
        // window unrevealable from the tray.
        reveal_main_window(
            || {},
            || Err::<(), String>("controller gone".into()),
            || {
                calls.borrow_mut().push("show");
                Ok::<(), String>(())
            },
            || {
                calls.borrow_mut().push("unminimize");
                Ok::<(), String>(())
            },
            || {
                calls.borrow_mut().push("focus");
                Ok::<(), String>(())
            },
            || calls.borrow_mut().push("foreground"),
        )
        .expect("a webview visibility failure must not fail the reveal");

        assert_eq!(
            *calls.borrow(),
            ["show", "unminimize", "foreground", "focus"]
        );
    }

    #[test]
    fn failed_focus_still_publishes_foreground_after_reveal() {
        let calls = std::cell::RefCell::new(Vec::new());

        let error = reveal_main_window(
            || calls.borrow_mut().push("memory_normal"),
            || {
                calls.borrow_mut().push("webview_show");
                Ok::<(), String>(())
            },
            || {
                calls.borrow_mut().push("show");
                Ok::<(), String>(())
            },
            || {
                calls.borrow_mut().push("unminimize");
                Ok::<(), String>(())
            },
            || {
                calls.borrow_mut().push("focus");
                Err::<(), String>("focus refused".into())
            },
            || calls.borrow_mut().push("foreground"),
        )
        .expect_err("focus failure should still be reported");

        assert!(error.contains("focus refused"));
        assert_eq!(
            *calls.borrow(),
            [
                "memory_normal",
                "webview_show",
                "show",
                "unminimize",
                "foreground",
                "focus"
            ]
        );
    }

    #[test]
    fn failed_native_reveal_steps_still_publish_foreground_and_attempt_recovery() {
        let calls = std::cell::RefCell::new(Vec::new());

        let error = reveal_main_window(
            || calls.borrow_mut().push("memory_normal"),
            || {
                calls.borrow_mut().push("webview_show");
                Ok::<(), String>(())
            },
            || {
                calls.borrow_mut().push("show");
                Err::<(), String>("show refused".into())
            },
            || {
                calls.borrow_mut().push("unminimize");
                Err::<(), String>("unminimize refused".into())
            },
            || {
                calls.borrow_mut().push("focus");
                Ok::<(), String>(())
            },
            || calls.borrow_mut().push("foreground"),
        )
        .expect_err("native reveal failures should still be reported");

        assert!(error.contains("show refused"));
        assert_eq!(
            *calls.borrow(),
            [
                "memory_normal",
                "webview_show",
                "show",
                "unminimize",
                "foreground",
                "focus"
            ],
            "frontend boot must not be gated on a fallible native reveal step"
        );
    }

    #[test]
    fn hiding_main_window_hides_native_window_before_webview() {
        let calls = std::cell::RefCell::new(Vec::new());

        hide_main_window(
            || {
                calls.borrow_mut().push("hide");
                Ok::<(), String>(())
            },
            || calls.borrow_mut().push("background"),
            || {
                calls.borrow_mut().push("webview_hide");
                Ok::<(), String>(())
            },
            || calls.borrow_mut().push("memory_low"),
        )
        .unwrap();

        assert_eq!(
            *calls.borrow(),
            ["hide", "background", "webview_hide", "memory_low"]
        );
    }

    #[test]
    fn failed_native_hide_leaves_the_webview_visible() {
        let webview_hidden = std::cell::Cell::new(false);
        let backgrounded = std::cell::Cell::new(false);
        let throttled = std::cell::Cell::new(false);

        let error = hide_main_window(
            || Err::<(), String>("hide refused".into()),
            || backgrounded.set(true),
            || {
                webview_hidden.set(true);
                Ok::<(), String>(())
            },
            || throttled.set(true),
        )
        .expect_err("a failed native hide must surface");

        assert!(error.contains("hide refused"));
        assert!(
            !backgrounded.get(),
            "a failed native hide must not publish background state"
        );
        assert!(
            !webview_hidden.get(),
            "hiding the webview behind a still-visible window would blank it"
        );
        assert!(
            !throttled.get(),
            "throttling a view the user can still see would be visible to them"
        );
    }

    #[test]
    fn hide_reports_success_when_webview_visibility_fails() {
        // The window is already in the tray, so a controller failure is not
        // worth failing the whole transition over — and the memory target
        // should still be lowered.
        let throttled = std::cell::Cell::new(false);

        hide_main_window(
            || Ok::<(), String>(()),
            || {},
            || Err::<(), String>("controller gone".into()),
            || throttled.set(true),
        )
        .expect("webview visibility is best-effort on hide");

        assert!(throttled.get());
    }

    #[test]
    fn window_lifecycle_revisions_only_change_with_native_mode() {
        let state = WindowLifecycleState::default();

        assert_eq!(
            state.snapshot(),
            WindowLifecycleSnapshot::new(0, WindowLifecycleMode::Destroyed)
        );
        assert_eq!(
            state.transition(WindowLifecycleMode::Destroyed),
            WindowLifecycleSnapshot::new(0, WindowLifecycleMode::Destroyed)
        );
        assert_eq!(
            state.transition(WindowLifecycleMode::Foreground),
            WindowLifecycleSnapshot::new(1, WindowLifecycleMode::Foreground)
        );
        assert_eq!(
            state.transition(WindowLifecycleMode::Taskbar),
            WindowLifecycleSnapshot::new(2, WindowLifecycleMode::Taskbar)
        );
    }

    #[test]
    fn taskbar_restore_restores_webview_before_publishing_foreground() {
        let calls = std::cell::RefCell::new(Vec::new());

        restore_taskbar_webview(
            || calls.borrow_mut().push("memory_normal"),
            || {
                calls.borrow_mut().push("webview_show");
                Ok::<(), String>(())
            },
            || calls.borrow_mut().push("foreground"),
        )
        .unwrap();

        assert_eq!(
            *calls.borrow(),
            ["memory_normal", "webview_show", "foreground"]
        );
    }

    #[test]
    fn failed_taskbar_webview_restore_does_not_publish_foreground() {
        let foreground = std::cell::Cell::new(false);

        let error = restore_taskbar_webview(
            || {},
            || Err::<(), String>("controller show failed".into()),
            || foreground.set(true),
        )
        .expect_err("controller show failure must keep taskbar lifecycle state");

        assert!(error.contains("controller show failed"));
        assert!(!foreground.get());
    }

    #[test]
    fn microphone_test_requires_foreground_window_lifecycle() {
        let state = WindowLifecycleState::default();
        assert!(ensure_foreground_microphone_test(&state).is_err());

        state.transition(WindowLifecycleMode::Foreground);
        assert!(ensure_foreground_microphone_test(&state).is_ok());

        state.transition(WindowLifecycleMode::Taskbar);
        assert!(ensure_foreground_microphone_test(&state).is_err());
    }

    #[test]
    fn native_minimize_fallback_requires_confirmed_minimized_state() {
        let calls = std::cell::RefCell::new(Vec::new());

        let changed = background_if_native_minimized(
            || Ok::<bool, String>(false),
            || calls.borrow_mut().push("background"),
            || {
                calls.borrow_mut().push("webview_hide");
                Ok::<(), String>(())
            },
            || calls.borrow_mut().push("memory_low"),
        )
        .unwrap();

        assert!(!changed);
        assert!(
            calls.borrow().is_empty(),
            "ordinary focus loss or Alt-Tab must not be treated as background"
        );
    }

    #[test]
    fn native_minimize_fallback_publishes_before_releasing_webview() {
        let calls = std::cell::RefCell::new(Vec::new());

        let changed = background_if_native_minimized(
            || Ok::<bool, String>(true),
            || calls.borrow_mut().push("background"),
            || {
                calls.borrow_mut().push("webview_hide");
                Ok::<(), String>(())
            },
            || calls.borrow_mut().push("memory_low"),
        )
        .unwrap();

        assert!(changed);
        assert_eq!(
            *calls.borrow(),
            ["background", "webview_hide", "memory_low"]
        );
    }

    #[test]
    fn resize_signal_reconciles_native_minimize_and_restore_without_focus() {
        let resize = WindowEvent::Resized(tauri::PhysicalSize::new(800, 600));

        assert!(should_reconcile_native_window_event(&resize));
        assert_eq!(
            native_window_reconcile_action(WindowLifecycleMode::Foreground, true),
            NativeWindowReconcileAction::BackgroundTaskbar
        );
        assert_eq!(
            native_window_reconcile_action(WindowLifecycleMode::Taskbar, false),
            NativeWindowReconcileAction::RestoreTaskbar
        );
    }

    #[test]
    fn native_window_reconciliation_ignores_stable_and_tray_states() {
        assert_eq!(
            native_window_reconcile_action(WindowLifecycleMode::Foreground, false),
            NativeWindowReconcileAction::None
        );
        assert_eq!(
            native_window_reconcile_action(WindowLifecycleMode::Taskbar, true),
            NativeWindowReconcileAction::None
        );
        assert_eq!(
            native_window_reconcile_action(WindowLifecycleMode::Tray, false),
            NativeWindowReconcileAction::None
        );
        assert_eq!(
            native_window_reconcile_action(WindowLifecycleMode::Tray, true),
            NativeWindowReconcileAction::None
        );
        assert_eq!(
            native_window_reconcile_action(WindowLifecycleMode::Destroying, true),
            NativeWindowReconcileAction::None
        );
        assert_eq!(
            native_window_reconcile_action(WindowLifecycleMode::Destroyed, false),
            NativeWindowReconcileAction::None
        );
    }

    #[test]
    fn diagnostic_window_event_filter_drops_move_and_resize_noise() {
        assert!(!should_log_window_event(&WindowEvent::Moved(
            tauri::PhysicalPosition::new(10, 20)
        )));
        assert!(!should_log_window_event(&WindowEvent::Resized(
            tauri::PhysicalSize::new(800, 600)
        )));
        assert!(should_log_window_event(&WindowEvent::Focused(true)));
        assert!(should_log_window_event(&WindowEvent::Destroyed));
    }

    #[test]
    fn disabled_stable_channel_cannot_check_updates_yet() {
        assert!(!UpdateChannel::Stable.enabled());
        assert!(UpdateChannel::Nightly.enabled());
    }

    #[test]
    fn missing_release_metadata_message_names_channel_workflow() {
        assert_eq!(
            missing_release_metadata_message(UpdateChannel::Nightly),
            "No Nightly release metadata is published yet. Publish a Nightly release first."
        );
    }

    #[test]
    fn active_full_session_game_sets_service_recording_mode() {
        let inner = RuntimeInner {
            tx: None,
            recording_generation: 0,
            recording_desired: false,
            manual_full_session_desired: false,
            settings: AppSettings::default(),
            lol_url: None,
            active_game: Some(DetectedGame {
                identity: crate::game_identity::GameIdentity::custom(
                    crate::game_plugins::LEAGUE_OF_LEGENDS_ID,
                ),
                name: "Game".into(),
                hwnd: 42,
                window_title: "Game Window".into(),
                process_id: 7,
                exe_name: "game.exe".into(),
                exe_path: None,
                recording_mode: GameRecordingMode::FullSession,
            }),
            osu_title_events: Vec::new(),
            last_save_request: None,
            decodable_codecs: vec![service::Codec::H264],
            last_recorder_status: None,
            last_storage_status: None,
            recent_recorder_error: false,
            quota_blocked: None,
        };

        let opts = RuntimeState::options(&inner).unwrap();

        assert_eq!(
            opts.active_game
                .as_ref()
                .and_then(|game| game.identity.plugin_id()),
            None
        );
        assert_eq!(opts.recording_mode, service::RecordingMode::FullSession);
        assert_eq!(
            opts.capture_source,
            service::CaptureSource::WindowHandle {
                hwnd: 42,
                title: "Game Window".into(),
            }
        );
    }

    #[test]
    fn active_built_in_game_sets_service_plugin_id_for_event_sources() {
        let inner = RuntimeInner {
            tx: None,
            recording_generation: 0,
            recording_desired: false,
            manual_full_session_desired: false,
            settings: AppSettings::default(),
            lol_url: Some("http://mock".into()),
            active_game: Some(DetectedGame {
                identity: crate::game_identity::GameIdentity::built_in_plugin(
                    crate::game_plugins::LEAGUE_OF_LEGENDS_ID,
                )
                .unwrap(),
                name: "League of Legends".into(),
                hwnd: 42,
                window_title: "League".into(),
                process_id: 7,
                exe_name: "League of Legends.exe".into(),
                exe_path: Some(
                    r"C:\Riot Games\League of Legends\Game\League of Legends.exe".into(),
                ),
                recording_mode: GameRecordingMode::FullSession,
            }),
            osu_title_events: Vec::new(),
            last_save_request: None,
            decodable_codecs: vec![service::Codec::H264],
            last_recorder_status: None,
            last_storage_status: None,
            recent_recorder_error: false,
            quota_blocked: None,
        };

        let opts = RuntimeState::options(&inner).unwrap();

        assert_eq!(
            opts.active_game
                .as_ref()
                .and_then(|game| game.identity.plugin_id()),
            Some(crate::game_plugins::LEAGUE_OF_LEGENDS_ID)
        );
        assert_eq!(opts.lol_url.as_deref(), Some("http://mock"));
        assert_eq!(
            opts.active_game.as_ref().and_then(|game| game.exe_path.as_deref()),
            Some(Path::new(
                r"C:\Riot Games\League of Legends\Game\League of Legends.exe"
            ))
        );
    }

    #[test]
    fn native_media_folder_authorization_is_exact_retryable_and_consumed_on_commit() {
        let authorization = NativeMediaFolderAuthorization::default();
        let old = PathBuf::from(r"C:\Users\tester\Videos\Clipline");
        let selected = PathBuf::from(r"D:\Recordings\Clipline");
        let other = PathBuf::from(r"D:\Other");

        assert!(authorization.validate_change(&old, &old).is_ok());
        assert!(authorization.validate_change(&old, &selected).is_err());

        authorization.authorize(selected.clone());
        assert!(authorization.validate_change(&old, &selected).is_ok());
        assert!(authorization.validate_change(&old, &selected).is_ok());
        assert!(authorization.validate_change(&old, &other).is_err());

        authorization.commit(&selected);
        assert!(authorization.validate_change(&old, &selected).is_err());
        assert!(authorization.validate_change(&selected, &selected).is_ok());
    }

    #[test]
    fn media_folder_display_path_removes_windows_verbatim_prefixes() {
        assert_eq!(
            display_media_folder_path(Path::new(r"\\?\C:\Users\tester\Videos\Clipline")),
            r"C:\Users\tester\Videos\Clipline"
        );
        assert_eq!(
            display_media_folder_path(Path::new(r"\\?\UNC\nas\clips")),
            r"\\nas\clips"
        );
        assert_eq!(
            display_media_folder_path(Path::new(r"D:\Clips")),
            r"D:\Clips"
        );
    }
}
