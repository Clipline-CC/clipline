//! Tauri shell: tray, Alt+F10 global hotkey, status webview — all thin
//! wiring around the recorder service thread.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use clipline_capture::diagnostics::install_diagnostic_handler;
use clipline_desktop::{
    Generation, Revision, UiAction, UiEffect, UiEvent, UiEventSink, WindowLifecycleMode,
    WindowLifecycleSnapshot,
};
use clipline_library::{ActiveFileRegistry, UploadService};
use clipline_shell::hotkey::{HotkeyReplacementReceipt, HotkeySet};
use clipline_shell::windows::activation::WindowsInstanceGuard;
use clipline_shell::windows::autostart::{AutostartChange, WindowsAutostartRegistration};
use clipline_shell::windows::hotkey::{AppliedHotkeyReplacement, WindowsHotkeyService};
use clipline_shell::{
    LaunchMode, SequencedShellCommand, ShellCommand, ShellCommandReceiver, ShellCommandSender,
    ShellLaunch, ShutdownAcknowledgement, ShutdownCoordinator, ShutdownEffect, ShutdownGate,
    ShutdownReason, WindowEffect, WindowEvent as ShellWindowEvent, WindowPolicy,
};
use clipline_updater::download::download_installer;
use clipline_updater::manifest::{
    check_update, installer_filename, UpdateCheck, UpdateManifest, UpdatePolicy, UpdateVariant,
};
use clipline_updater::windows::WindowsInstallerLauncher;
use clipline_updater::{
    install_verified, verify_download, UpdateOperationGate, UpdateOperationKind, UpdateShutdown,
};
use tauri::image::Image;
use tauri::menu::{Menu, MenuItem};
use tauri::path::BaseDirectory;
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, Runtime, WebviewWindow, WebviewWindowBuilder, WindowEvent};

use crate::game_discovery::DetectedGameCandidate;
use crate::game_plugins::GamePluginInfo;
use crate::games::{DetectedGame, GameWindowInfo};
use crate::osu_enrichment::OsuTitleEvent;
use crate::service::{self, Cmd, Event, ServiceOptions};
use crate::settings::{
    cloud_paths_equivalent, quota_bytes_from_gb, AppSettings, AppSettingsServiceExt, CaptureMode,
    CloudRecordCas, CloudRecordCasKind, CloudRecordSlot, CustomGameSettings, GameRecordingMode,
    SettingsApplyCoordinator, SettingsApplyPorts, SettingsChange, SettingsPreferences,
    SettingsProfile, SettingsSnapshot, SettingsStore, SettingsTransaction,
    MAX_CLOUD_RECORD_CAS_SLOTS,
};
use crate::updates::{channel_enabled, UpdateChannel};
use crate::util::unix_now_i64;

#[path = "app/diagnostics.rs"]
mod diagnostics;
#[path = "app/support.rs"]
mod support;
use diagnostics::{diagnostic_log_path, log_diagnostic};

const MAIN_WINDOW_LABEL: &str = "main";
const AUTOSTART_VALUE_NAME: &str = "Clipline";
const SHELL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(15);
const RECORDER_FINALIZATION_TIMEOUT: Duration = Duration::from_secs(10);
const UPLOAD_FINALIZATION_TIMEOUT: Duration = Duration::from_secs(5);
const UPDATE_QUIESCE_TIMEOUT: Duration = Duration::from_secs(21);
const WEBVIEW_READY_TIMEOUT: Duration = Duration::from_secs(5);
const GAME_DETECTOR_INTERVAL: Duration = Duration::from_millis(500);
static FRONTEND_READY: AtomicBool = AtomicBool::new(false);
static WEBVIEW_READY_WATCHDOG_ARMED: AtomicBool = AtomicBool::new(false);
static WEBVIEW_REPAIR_NOTICE_SHOWN: AtomicBool = AtomicBool::new(false);

pub(crate) struct WindowLifecycleState(Mutex<WindowLifecycleSnapshot>);

impl Default for WindowLifecycleState {
    fn default() -> Self {
        // The configured native window starts hidden. A normal launch moves to
        // Foreground after reveal; autostart deliberately remains in Tray.
        Self(Mutex::new(WindowLifecycleSnapshot::new(
            Revision::INITIAL,
            WindowLifecycleMode::Tray,
        )))
    }
}

impl WindowLifecycleState {
    pub(crate) fn snapshot(&self) -> WindowLifecycleSnapshot {
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
            let revision = Revision::new(snapshot.revision.get().saturating_add(1));
            *snapshot = WindowLifecycleSnapshot::new(revision, mode);
        }
        *snapshot
    }
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
    desktop_lifecycle_revision: String,
    desktop_notices: Vec<crate::desktop::tauri_sink::DesktopNoticePresentation>,
    desktop_snapshot: clipline_desktop::DesktopSnapshot<AppSettings>,
    desktop_event_sequence: u64,
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

    fn into_ui_event(self) -> clipline_desktop::GameDetection {
        clipline_desktop::GameDetection {
            active: self.active,
            name: self.name,
            window_title: self.window_title,
            process_id: self.process_id,
            process_instance_id: self.process_instance_id,
            exe_name: self.exe_name,
            recording_mode: self.recording_mode.map(|mode| match mode {
                GameRecordingMode::FullSession => "full_session".to_owned(),
                GameRecordingMode::ReplaysOnly => "replays_only".to_owned(),
            }),
            elevated_hotkeys_blocked: self.elevated_hotkeys_blocked,
        }
    }
}

fn publish_game_detection<R: Runtime>(
    app: &AppHandle<R>,
    event: GameDetectionEvent,
) -> Result<(), String> {
    let generation = app
        .state::<crate::desktop::ProducerGenerations>()
        .next_game_detection()?;
    app.state::<crate::desktop::tauri_sink::TauriUiEventSink>()
        .try_publish(UiEvent::GameDetection {
            generation,
            detection: event.into_ui_event(),
        })
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn publish_user_error<R: Runtime>(app: &AppHandle<R>, message: String) {
    if let Err(error) = app
        .state::<crate::desktop::tauri_sink::TauriUiEventSink>()
        .try_publish(UiEvent::UserError { message })
    {
        tracing::error!(event = "user_error_publish_failed", error = %error);
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
enum MainWindowOpenTarget {
    ExistingMain,
    NewMain,
}

fn main_window_open_target(main_window_present: bool) -> MainWindowOpenTarget {
    if main_window_present {
        MainWindowOpenTarget::ExistingMain
    } else {
        MainWindowOpenTarget::NewMain
    }
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

fn arm_frontend_ready_watchdog() {
    if FRONTEND_READY.load(Ordering::Acquire) {
        return;
    }
    if WEBVIEW_READY_WATCHDOG_ARMED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
        .is_err()
    {
        return;
    }

    log_diagnostic("webview readiness watchdog armed");
    let _ = std::thread::Builder::new()
        .name("clipline-webview-readiness-watchdog".into())
        .spawn(|| {
            std::thread::sleep(WEBVIEW_READY_TIMEOUT);
            if !FRONTEND_READY.load(Ordering::Acquire) {
                log_diagnostic("webview readiness watchdog expired before frontend_ready");
                show_webview_repair_notice_once(WebviewRepairNoticeReason::FrontendReadyTimeout);
            } else {
                log_diagnostic("webview readiness watchdog observed frontend_ready");
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
    desktop: tauri::State<crate::desktop::DesktopState>,
    startup_warnings: tauri::State<StartupWarnings>,
    window_lifecycle: tauri::State<WindowLifecycleState>,
) -> FrontendReadyResponse {
    let was_ready = FRONTEND_READY.swap(true, Ordering::AcqRel);
    if !was_ready {
        log_diagnostic("frontend_ready received");
    }
    let recorder_status = runtime.current_recorder_status();
    if let Some((generation, status)) = recorder_status.clone() {
        if let Err(error) = desktop.apply_event(UiEvent::Recorder {
            generation: Generation::new(generation),
            event: status.clone(),
        }) {
            log_diagnostic(format!("desktop recorder reconciliation failed: {error}"));
        }
        let _ = app
            .state::<crate::desktop::tauri_sink::TauriUiEventSink>()
            .try_publish(UiEvent::Recorder {
                generation: Generation::new(generation),
                event: status,
            });
    }
    if let Err(error) = desktop.replace_settings(runtime.settings()) {
        log_diagnostic(format!("desktop settings reconciliation failed: {error}"));
    }
    let lifecycle = window_lifecycle.snapshot();
    if let Err(error) = desktop.apply_event(UiEvent::WindowLifecycle {
        snapshot: lifecycle,
    }) {
        log_diagnostic(format!("desktop lifecycle reconciliation failed: {error}"));
    }
    let bootstrap = desktop.bootstrap();
    let desktop_notices =
        crate::desktop::tauri_sink::pending_notice_presentations(&bootstrap.snapshot);
    FrontendReadyResponse {
        warnings: startup_warnings.take(),
        window_lifecycle: lifecycle,
        desktop_lifecycle_revision: lifecycle.revision.get().to_string(),
        desktop_notices,
        desktop_snapshot: bootstrap.snapshot,
        desktop_event_sequence: bootstrap.event_sequence,
    }
}

#[tauri::command]
fn acknowledge_desktop_notice(
    desktop: tauri::State<crate::desktop::DesktopState>,
    window_lifecycle: tauri::State<WindowLifecycleState>,
    notice_id: String,
    lifecycle_revision: String,
) -> Result<bool, String> {
    let notice_id = notice_id
        .parse::<u64>()
        .map_err(|_| "desktop notice identifier is invalid".to_owned())?;
    let lifecycle_revision = lifecycle_revision
        .parse::<u64>()
        .map(Revision::new)
        .map_err(|_| "desktop lifecycle revision is invalid".to_owned())?;

    // Hold the lifecycle lock through controller dispatch. A concurrent tray
    // transition therefore wins either entirely before or entirely after this
    // exact foreground acknowledgement; it can never race through the fence.
    let lifecycle = match window_lifecycle.0.lock() {
        Ok(lifecycle) => lifecycle,
        Err(poisoned) => poisoned.into_inner(),
    };
    if lifecycle.mode != WindowLifecycleMode::Foreground || lifecycle.revision != lifecycle_revision
    {
        return Ok(false);
    }
    desktop.acknowledge_notice_if_lifecycle(notice_id, *lifecycle)
}

#[derive(Default)]
struct StartupWarnings(Mutex<Vec<String>>);

impl StartupWarnings {
    fn new(warnings: Vec<String>) -> Self {
        Self(Mutex::new(warnings))
    }

    fn take(&self) -> Vec<String> {
        match self.0.lock() {
            Ok(mut warnings) => std::mem::take(&mut *warnings),
            Err(error) => vec![format!(
                "startup diagnostics could not be read because their lock was poisoned: {error}"
            )],
        }
    }
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
        let mut pending = match self.0.lock() {
            Ok(pending) => pending,
            Err(poisoned) => poisoned.into_inner(),
        };
        if pending
            .as_deref()
            .is_some_and(|authorized| same_path(authorized, path))
        {
            *pending = None;
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
        inner.last_generation = checked_generation_next(inner.last_generation, "microphone test")?;
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

fn checked_generation_next(current: u64, domain: &str) -> Result<u64, String> {
    current
        .checked_add(1)
        .ok_or_else(|| format!("{domain} generation exhausted"))
}

pub(crate) struct RuntimeState(
    Mutex<RuntimeInner>,
    RecorderStopAcknowledgement,
    Option<SettingsStore>,
    Mutex<Option<JoinHandle<()>>>,
);

#[derive(Default)]
struct RecorderStopAcknowledgement {
    generation: Mutex<Option<(u64, bool)>>,
    changed: Condvar,
}

impl RecorderStopAcknowledgement {
    fn publish(&self, generation: u64, finalized_cleanly: bool) {
        if let Ok(mut acknowledged) = self.generation.lock() {
            *acknowledged = Some((generation, finalized_cleanly));
            self.changed.notify_all();
        }
    }
}

static CLOUD_SETTINGS_SAVE_LOCK: Mutex<()> = Mutex::new(());

struct TrayItems<R: Runtime> {
    save_item: MenuItem<R>,
    hotkey_label: Mutex<String>,
}

struct TrayLabelReceipt {
    before: String,
    after: String,
}

impl<R: Runtime> TrayItems<R> {
    fn set_hotkey_label(&self, label: &str) -> Result<(), String> {
        self.save_item
            .set_text(save_menu_text(label))
            .map_err(|e| e.to_string())?;
        *self
            .hotkey_label
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = label.to_owned();
        Ok(())
    }

    fn replace_hotkey_label(&self, label: &str) -> Result<TrayLabelReceipt, String> {
        let before = self
            .hotkey_label
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        self.set_hotkey_label(label)?;
        Ok(TrayLabelReceipt {
            before,
            after: label.to_owned(),
        })
    }

    fn rollback_hotkey_label(&self, receipt: TrayLabelReceipt) -> Result<(), String> {
        let current = self
            .hotkey_label
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if current != receipt.after {
            return Err("tray hotkey label changed concurrently; refusing to overwrite it".into());
        }
        self.set_hotkey_label(&receipt.before)
    }
}

struct RuntimeInner {
    tx: Option<Sender<Cmd>>,
    recording_generation: u64,
    recording_desired: bool,
    settings: AppSettings,
    active_files: ActiveFileRegistry,
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
}

#[derive(Clone)]
struct RecorderDiagnosticStatus {
    generation: u64,
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
    #[cfg(test)]
    settings: AppSettings,
    options: Option<ServiceOptions>,
    worker: Option<service::PreparedRecorderRestart>,
}

struct PreparedServiceRestart {
    old_tx: Option<Sender<Cmd>>,
    replacement: Option<(ServiceOptions, u64)>,
    waiting_for_game: bool,
    waiting_generation: Option<u64>,
}

#[cfg(test)]
#[derive(Debug)]
struct CommittedRuntimeRestart<T> {
    old_tx: Option<Sender<Cmd>>,
    replacement: Option<(T, u64)>,
    waiting_for_game: bool,
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

fn emit_waiting_for_game<R: Runtime>(app: &AppHandle<R>, generation: u64) {
    let _ = app
        .state::<crate::desktop::tauri_sink::TauriUiEventSink>()
        .try_publish(UiEvent::Recorder {
            generation: Generation::new(generation),
            event: waiting_for_game_status(),
        });
}

impl RuntimeState {
    #[cfg(test)]
    fn new(settings: AppSettings, lol_url: Option<String>) -> Self {
        Self::from_parts(None, settings, lol_url, None)
    }

    #[cfg(test)]
    fn with_store(settings: AppSettings, lol_url: Option<String>, store: SettingsStore) -> Self {
        Self::from_parts(None, settings, lol_url, Some(store))
    }

    fn with_store_and_registry(
        settings: AppSettings,
        lol_url: Option<String>,
        store: SettingsStore,
        active_files: ActiveFileRegistry,
    ) -> Self {
        Self::from_parts_with_registry(None, settings, lol_url, Some(store), active_files)
    }

    #[cfg(test)]
    fn with_sender(tx: Sender<Cmd>, settings: AppSettings, lol_url: Option<String>) -> Self {
        Self::from_parts(Some(tx), settings, lol_url, None)
    }

    #[cfg(test)]
    fn from_parts(
        tx: Option<Sender<Cmd>>,
        settings: AppSettings,
        lol_url: Option<String>,
        store: Option<SettingsStore>,
    ) -> Self {
        Self::from_parts_with_registry(tx, settings, lol_url, store, ActiveFileRegistry::new())
    }

    fn from_parts_with_registry(
        tx: Option<Sender<Cmd>>,
        settings: AppSettings,
        lol_url: Option<String>,
        store: Option<SettingsStore>,
        active_files: ActiveFileRegistry,
    ) -> Self {
        let mut inner = RuntimeInner {
            tx: None,
            recording_generation: 0,
            recording_desired: false,
            settings,
            active_files,
            lol_url,
            active_game: None,
            osu_title_events: Vec::new(),
            last_save_request: None,
            decodable_codecs: vec![service::Codec::H264],
            last_recorder_status: None,
            last_storage_status: None,
            recent_recorder_error: false,
        };
        if let Some(tx) = tx {
            Self::install_recording_sender(&mut inner, tx)
                .expect("initial recording generation is available");
        }
        Self(
            Mutex::new(inner),
            RecorderStopAcknowledgement::default(),
            store,
            Mutex::new(None),
        )
    }

    fn take_event_pump(&self) -> Option<JoinHandle<()>> {
        self.3
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }

    fn install_event_pump(&self, pump: JoinHandle<()>) {
        let replaced = self
            .3
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .replace(pump);
        debug_assert!(
            replaced.is_none(),
            "event pump ownership must be joined first"
        );
        if let Some(replaced) = replaced {
            let _ = replaced.join();
        }
    }

    fn install_recording_sender(inner: &mut RuntimeInner, tx: Sender<Cmd>) -> Result<u64, String> {
        inner.recording_generation =
            checked_generation_next(inner.recording_generation, "recording")?;
        inner.recording_desired = true;
        inner.tx = Some(tx);
        inner.last_save_request = None;
        Ok(inner.recording_generation)
    }

    fn accept_service_status(&self, generation: u64, recording: bool) -> bool {
        let Ok(mut inner) = self.0.lock() else {
            return false;
        };
        if inner.recording_generation != generation || inner.tx.is_none() {
            return false;
        }
        let finalized_cleanly = !inner.recent_recorder_error;
        if !recording {
            let Ok(next_generation) =
                checked_generation_next(inner.recording_generation, "recording")
            else {
                return false;
            };
            inner.tx = None;
            inner.recording_desired = false;
            inner.recording_generation = next_generation;
            inner.last_save_request = None;
        }
        drop(inner);
        if !recording {
            self.1.publish(generation, finalized_cleanly);
        }
        true
    }

    fn stop_recorder_and_wait(&self, timeout: Duration) -> Result<(), String> {
        let (sender, generation) = {
            let inner = self
                .0
                .lock()
                .map_err(|_| "runtime state lock poisoned".to_string())?;
            let Some(sender) = inner.tx.as_ref().cloned() else {
                return Ok(());
            };
            (sender, inner.recording_generation)
        };
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| "recorder finalization deadline overflowed".to_string())?;
        let mut acknowledged = self
            .1
            .generation
            .lock()
            .map_err(|_| "recorder finalization acknowledgement lock poisoned".to_string())?;
        sender
            .send(Cmd::Stop { announce: true })
            .map_err(|_| "recorder stop channel closed before finalization".to_string())?;

        loop {
            if let Some((acknowledged_generation, finalized_cleanly)) = *acknowledged {
                if acknowledged_generation == generation {
                    let result = if finalized_cleanly {
                        Ok(())
                    } else {
                        Err("recorder reported a finalization failure".to_string())
                    };
                    drop(acknowledged);
                    if let Some(pump) = self.take_event_pump() {
                        let _ = pump.join();
                    }
                    return result;
                }
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(format!(
                    "recorder finalization timed out after {} ms",
                    timeout.as_millis()
                ));
            }
            let (next, wait) = self
                .1
                .changed
                .wait_timeout(acknowledged, remaining)
                .map_err(|_| "recorder finalization acknowledgement lock poisoned".to_string())?;
            acknowledged = next;
            if wait.timed_out()
                && !matches!(*acknowledged, Some((acknowledged_generation, _)) if acknowledged_generation == generation)
            {
                return Err(format!(
                    "recorder finalization timed out after {} ms",
                    timeout.as_millis()
                ));
            }
        }
    }

    fn service_generation_is_current(&self, generation: u64) -> bool {
        self.0
            .lock()
            .is_ok_and(|inner| inner.recording_generation == generation && inner.tx.is_some())
    }

    fn observe_runtime_event(&self, generation: u64, event: &Event) {
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
                    generation,
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
            Event::Error { .. } => inner.recent_recorder_error = true,
            Event::MediaRootResolved { .. } => {}
        }
    }

    fn current_waiting_status(&self) -> Option<(u64, Event)> {
        let inner = self.0.lock().ok()?;
        (inner.recording_desired
            && inner.tx.is_none()
            && !recorder_should_run(&inner.settings, inner.active_game.as_ref()))
        .then(|| (inner.recording_generation, waiting_for_game_status()))
    }

    fn current_recorder_status(&self) -> Option<(u64, Event)> {
        let inner = self.0.lock().ok()?;
        if inner.recording_desired
            && inner.tx.is_none()
            && !recorder_should_run(&inner.settings, inner.active_game.as_ref())
        {
            return Some((inner.recording_generation, waiting_for_game_status()));
        }
        if !inner.recording_desired {
            return Some((
                inner.recording_generation,
                Event::Status {
                    recording: false,
                    waiting_for_game: false,
                    segments: 0,
                    buffered_s: 0.0,
                    buffered_mb: 0.0,
                    full_session: false,
                    encoder: String::new(),
                    capture_backend: String::new(),
                },
            ));
        }
        let status = inner
            .last_recorder_status
            .as_ref()
            .filter(|status| status.generation == inner.recording_generation)?;
        Some((
            status.generation,
            Event::Status {
                recording: status.recording,
                waiting_for_game: status.waiting_for_game,
                segments: status.segments,
                buffered_s: status.buffered_s,
                buffered_mb: status.buffered_mb,
                full_session: status.full_session,
                encoder: status.encoder.clone(),
                capture_backend: status.capture_backend.clone(),
            },
        ))
    }

    fn waiting_generation_is_current(&self, generation: u64) -> bool {
        self.0.lock().is_ok_and(|inner| {
            inner.recording_generation == generation
                && inner.recording_desired
                && inner.tx.is_none()
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
        active_files: ActiveFileRegistry,
    ) -> Result<service::ServiceOptions, String> {
        let mut opts = settings.to_service_options_with_registry(lol_url, active_files)?;
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
            });
        }
        Ok(opts)
    }

    fn options(inner: &RuntimeInner) -> Result<service::ServiceOptions, String> {
        Self::options_for(
            &inner.settings,
            inner.lol_url.clone(),
            inner.active_game.as_ref(),
            &inner.decodable_codecs,
            inner.active_files.clone(),
        )
    }

    fn prepare_service_restart(inner: &mut RuntimeInner) -> Result<PreparedServiceRestart, String> {
        let should_run = inner.recording_desired
            && recorder_should_run(&inner.settings, inner.active_game.as_ref());
        let next_options = if should_run {
            let mut options = match Self::options(inner) {
                Ok(options) => options,
                Err(error) => {
                    // A sender means the current service is still authoritative,
                    // so preserve it on an option error. With no sender, a prior
                    // restart is already spawning; invalidate that stale plan.
                    if inner.tx.is_none() {
                        inner.recording_generation =
                            checked_generation_next(inner.recording_generation, "recording")?;
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
        inner.recording_generation =
            checked_generation_next(inner.recording_generation, "recording")?;
        let generation = inner.recording_generation;
        inner.last_save_request = None;
        Ok(PreparedServiceRestart {
            old_tx,
            replacement: next_options.map(|options| (options, generation)),
            waiting_for_game: inner.recording_desired && !should_run,
            waiting_generation: (inner.recording_desired && !should_run).then_some(generation),
        })
    }

    fn install_prepared_service_restart(
        inner: &mut RuntimeInner,
        generation: u64,
        tx: Sender<Cmd>,
    ) -> Result<u64, Sender<Cmd>> {
        if !inner.recording_desired
            || inner.recording_generation != generation
            || inner.tx.is_some()
        {
            return Err(tx);
        }
        let Ok(generation) = checked_generation_next(inner.recording_generation, "recording")
        else {
            return Err(tx);
        };
        inner.recording_generation = generation;
        inner.recording_desired = true;
        inner.tx = Some(tx);
        inner.last_save_request = None;
        Ok(generation)
    }

    fn install_settings_restart_sender(
        inner: &mut RuntimeInner,
        generation: u64,
        tx: Sender<Cmd>,
    ) -> Result<(), Sender<Cmd>> {
        if inner.recording_generation != generation
            || !inner.recording_desired
            || inner.tx.is_some()
            || !recorder_should_run(&inner.settings, inner.active_game.as_ref())
        {
            return Err(tx);
        }
        inner.tx = Some(tx);
        inner.last_save_request = None;
        Ok(())
    }

    fn prepare_settings_restart(
        &self,
        settings: AppSettings,
    ) -> Result<PreparedRuntimeRestart, String> {
        let mut options = {
            let inner = self.0.lock().map_err(|_| "runtime state lock poisoned")?;
            Self::options_for(
                &settings,
                inner.lol_url.clone(),
                None,
                &inner.decodable_codecs,
                inner.active_files.clone(),
            )?
        };
        options.recover_abandoned_recordings = false;
        let worker = service::PreparedRecorderRestart::prepare(options.clone())?;
        Ok(PreparedRuntimeRestart {
            #[cfg(test)]
            settings,
            options: Some(options),
            worker: Some(worker),
        })
    }

    #[cfg(test)]
    fn commit_prepared_restart_with<T, F>(
        inner: &mut RuntimeInner,
        prepared: PreparedRuntimeRestart,
        spawn: F,
    ) -> Result<CommittedRuntimeRestart<T>, String>
    where
        F: FnOnce(ServiceOptions) -> (Sender<Cmd>, T),
    {
        let PreparedRuntimeRestart { settings, .. } = prepared;
        let cleared_active_game = inner.active_game.is_some()
            && !active_game_still_configured(&settings, inner.active_game.as_ref());
        let active_game = if cleared_active_game {
            None
        } else {
            inner.active_game.as_ref()
        };
        let should_run = inner.recording_desired && recorder_should_run(&settings, active_game);
        let next_options = if should_run {
            let mut options = Self::options_for(
                &settings,
                inner.lol_url.clone(),
                active_game,
                &inner.decodable_codecs,
                inner.active_files.clone(),
            )?;
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
            let generation = Self::install_recording_sender(inner, tx)?;
            Some((spawned, generation))
        } else {
            None
        };
        let waiting_for_game = inner.recording_desired && !should_run;
        if waiting_for_game {
            inner.recording_generation =
                checked_generation_next(inner.recording_generation, "recording")?;
            inner.last_save_request = None;
        }
        Ok(CommittedRuntimeRestart {
            old_tx,
            replacement,
            waiting_for_game,
        })
    }

    fn finish_prepared_restart<R: Runtime>(
        &self,
        app: AppHandle<R>,
        mut prepared: PreparedRuntimeRestart,
        authoritative: AppSettings,
    ) {
        let mut options = prepared
            .options
            .take()
            .expect("prepared settings restart owns validated options");
        let mut sender = prepared
            .worker
            .as_ref()
            .map(service::PreparedRecorderRestart::command_sender);
        let (old_tx, replacement_generation, cleared_active_game, waiting_for_game) = {
            let mut inner = self
                .0
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let cleared_active_game = inner.active_game.is_some()
                && !active_game_still_configured(&authoritative, inner.active_game.as_ref());
            if cleared_active_game {
                inner.active_game = None;
            }
            options.lol_url.clone_from(&inner.lol_url);
            options.decodable_codecs.clone_from(&inner.decodable_codecs);
            options.active_files = inner.active_files.clone();
            if let Some(game) = inner.active_game.as_ref() {
                options.capture_source = service::CaptureSource::WindowHandle {
                    hwnd: game.hwnd,
                    title: game.window_title.clone(),
                };
                options.recording_mode = game.recording_mode.into();
                options.active_game = Some(service::ActiveGame {
                    identity: game.identity.clone(),
                    name: game.name.clone(),
                });
            }
            let should_run = inner.recording_desired
                && recorder_should_run(&authoritative, inner.active_game.as_ref());
            inner.settings = authoritative;
            let old_tx = inner.tx.take();
            let replacement_generation = if old_tx.is_some() || inner.recording_desired {
                inner.recording_generation.checked_add(1)
            } else {
                None
            };
            if let Some(generation) = replacement_generation {
                inner.recording_generation = generation;
            }
            inner.last_save_request = None;
            let replacement_generation = should_run.then_some(replacement_generation).flatten();
            if should_run && replacement_generation.is_none() {
                inner.recording_desired = false;
            }
            (
                old_tx,
                replacement_generation,
                cleared_active_game,
                inner.recording_desired && !should_run,
            )
        };
        if let Some(tx) = old_tx {
            let _ = tx.send(Cmd::Stop { announce: false });
        }
        if let Some(old_pump) = self.take_event_pump() {
            let _ = old_pump.join();
        }
        if let Some(generation) = replacement_generation {
            let mut inner = self
                .0
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            // Joining the prior recorder can take long enough for Stop/Start
            // or game detection to install a newer generation. Publish and
            // release this parked worker only while its exact reservation is
            // still current; otherwise Drop cancels and joins it.
            let tx = sender
                .take()
                .expect("running prepared restart owns a command sender");
            if Self::install_settings_restart_sender(&mut inner, generation, tx).is_ok() {
                let stream = prepared
                    .worker
                    .take()
                    .expect("running prepared restart owns a worker")
                    .commit_with_options(options);
                self.install_event_pump(pump_events(app.clone(), stream, generation));
            }
        }
        if waiting_for_game {
            let generation = self
                .0
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .recording_generation;
            if self.waiting_generation_is_current(generation) {
                emit_waiting_for_game(&app, generation);
            }
        }
        if cleared_active_game {
            if let Err(error) =
                publish_game_detection(&app, GameDetectionEvent::from_detected(None))
            {
                log_diagnostic(format!(
                    "publish cleared game after settings commit failed: {error}"
                ));
            }
        }
    }

    fn request_save(&self) -> bool {
        const DOUBLE_TRIGGER_DEBOUNCE: Duration = Duration::from_millis(150);

        if let Ok(mut inner) = self.0.lock() {
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

    pub(crate) fn cloud_settings_generation(
        &self,
    ) -> Result<(crate::settings::CloudSettings, u64), String> {
        if let Some(store) = self.2.as_ref() {
            let snapshot = store.snapshot().map_err(|error| error.to_string())?;
            return Ok((snapshot.document.cloud, snapshot.account_generation.get()));
        }
        Ok((self.settings().cloud, 1))
    }

    /// Read the exact durable settings/account revision used by Cloud record
    /// compare-and-swap adapters. Keeping this behind the same serialization
    /// lock as Cloud writes prevents an adapter from pairing a record snapshot
    /// with a concurrently replaced account generation.
    pub(crate) fn cloud_settings_snapshot(&self) -> Result<SettingsSnapshot, String> {
        let _save_guard = Self::lock_cloud_settings_save()?;
        self.2
            .as_ref()
            .ok_or_else(|| "cloud settings snapshot requires a durable settings store".to_string())?
            .snapshot()
            .map_err(|error| error.to_string())
    }

    pub(crate) fn with_cloud_settings_exclusive<T>(
        &self,
        operation: impl FnOnce(&crate::settings::CloudSettings, u64) -> Result<T, String>,
    ) -> Result<T, String> {
        let _save_guard = Self::lock_cloud_settings_save()?;
        if let Some(store) = self.2.as_ref() {
            let snapshot = store.snapshot().map_err(|error| error.to_string())?;
            return operation(&snapshot.document.cloud, snapshot.account_generation.get());
        }
        let cloud = self.settings().cloud;
        operation(&cloud, 1)
    }

    pub(crate) fn replace_cloud_profile_if_generation(
        &self,
        expected_generation: u64,
        mut cloud: crate::settings::CloudSettings,
    ) -> Result<AppSettings, String> {
        cloud.normalize();
        let Some(store) = self.2.as_ref() else {
            if expected_generation != 1 {
                return Err("cloud account changed while profile work was in flight".into());
            }
            return self.update_cloud_with(|current| *current = cloud, AppSettings::save);
        };
        let _save_guard = Self::lock_cloud_settings_save()?;
        let before = store.snapshot().map_err(|error| error.to_string())?;
        if before.account_generation.get() != expected_generation {
            return Err("cloud account changed while profile work was in flight".into());
        }
        let after = store
            .transact(SettingsTransaction {
                expected_revision: before.revision,
                expected_account_generation: before.account_generation,
                change: SettingsChange::ReplaceCloudProfile(cloud),
            })
            .map_err(|error| error.to_string())?;
        let mut inner = self.0.lock().map_err(|_| "runtime state lock poisoned")?;
        inner.settings.cloud = after.document.cloud;
        Ok(inner.settings.clone())
    }

    pub(crate) fn update_cloud<F>(&self, update: F) -> Result<AppSettings, String>
    where
        F: FnOnce(&mut crate::settings::CloudSettings),
    {
        let Some(store) = self.2.as_ref() else {
            return self.update_cloud_with(update, AppSettings::save);
        };
        let _save_guard = Self::lock_cloud_settings_save()?;
        let before = store.snapshot().map_err(|error| error.to_string())?;
        let mut cloud = before.document.cloud.clone();
        update(&mut cloud);
        cloud.normalize();
        let after = store
            .transact(SettingsTransaction {
                expected_revision: before.revision,
                expected_account_generation: before.account_generation,
                change: SettingsChange::ReplaceCloudSettings(cloud),
            })
            .map_err(|error| error.to_string())?;
        let mut inner = self.0.lock().map_err(|_| "runtime state lock poisoned")?;
        inner.settings.cloud = after.document.cloud;
        Ok(inner.settings.clone())
    }

    pub(crate) fn compare_exchange_cloud_records(
        &self,
        change: CloudRecordCas,
    ) -> Result<SettingsSnapshot, String> {
        let _save_guard = Self::lock_cloud_settings_save()?;
        let Some(store) = self.2.as_ref() else {
            return Err("cloud record CAS requires a durable settings store".into());
        };

        let before = store.snapshot().map_err(|error| error.to_string())?;
        let after = store
            .transact(SettingsTransaction {
                expected_revision: before.revision,
                expected_account_generation: before.account_generation,
                change: SettingsChange::CompareExchangeCloudRecords(change),
            })
            .map_err(|error| error.to_string())?;
        let mut inner = self.0.lock().map_err(|_| "runtime state lock poisoned")?;
        inner.settings.cloud = after.document.cloud.clone();
        Ok(after)
    }

    /// Reconcile every exact Windows-path alias produced by a successful local
    /// rename in one whole-settings transaction. Unrelated Cloud records and
    /// settings fields are retained from the same revision.
    pub(crate) fn reconcile_cloud_record_path(
        &self,
        old_path: &str,
        new_path: &str,
    ) -> Result<bool, String> {
        if old_path == new_path {
            return Ok(false);
        }
        let _save_guard = Self::lock_cloud_settings_save()?;
        let Some(store) = self.2.as_ref() else {
            return Err("cloud path reconciliation requires a durable settings store".into());
        };
        let before = store.snapshot().map_err(|error| error.to_string())?;
        let mut matches = before
            .document
            .cloud
            .uploads
            .iter()
            .filter(|(_, record)| cloud_paths_equivalent(&record.path, old_path))
            .map(|(key, record)| (key.clone(), record.clone()))
            .collect::<Vec<_>>();
        if matches.is_empty() {
            return Ok(false);
        }
        if matches.len() > MAX_CLOUD_RECORD_CAS_SLOTS {
            return Err(format!(
                "cloud rename matched {} path aliases; maximum is {MAX_CLOUD_RECORD_CAS_SLOTS}",
                matches.len()
            ));
        }
        matches.sort_by(|left, right| {
            left.1
                .updated_at_unix
                .cmp(&right.1.updated_at_unix)
                .then_with(|| left.0.cmp(&right.0))
        });
        let (stable_key, mut replacement) = matches
            .last()
            .cloned()
            .expect("non-empty Cloud path matches have a newest record");
        if stable_key != replacement.local_clip_id {
            return Err("cloud rename record key does not match its stable local identity".into());
        }
        replacement.path = new_path.to_string();
        replacement.normalize();
        let change = CloudRecordCas {
            account: before.account.clone(),
            account_generation: before.account_generation,
            kind: CloudRecordCasKind::StatusSync,
            expected: matches
                .into_iter()
                .map(|(key, record)| CloudRecordSlot {
                    key,
                    record: Some(record),
                })
                .collect(),
            replacement: Some(CloudRecordSlot {
                key: stable_key,
                record: Some(replacement),
            }),
        };
        let after = store
            .transact(SettingsTransaction {
                expected_revision: before.revision,
                expected_account_generation: before.account_generation,
                change: SettingsChange::CompareExchangeCloudRecords(change),
            })
            .map_err(|error| error.to_string())?;
        let mut inner = self.0.lock().map_err(|_| "runtime state lock poisoned")?;
        inner.settings.cloud = after.document.cloud;
        Ok(true)
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
        let Some(store) = self.2.as_ref() else {
            return self.update_osu_with(update, AppSettings::save);
        };
        let _save_guard = Self::lock_cloud_settings_save()?;
        let before = store.snapshot().map_err(|error| error.to_string())?;
        let mut osu = before.document.osu.clone();
        update(&mut osu);
        osu.normalize();
        let after = store
            .transact(SettingsTransaction {
                expected_revision: before.revision,
                expected_account_generation: before.account_generation,
                change: SettingsChange::ReplaceOsuProfile(osu),
            })
            .map_err(|error| error.to_string())?;
        let mut inner = self.0.lock().map_err(|_| "runtime state lock poisoned")?;
        inner.settings.osu = after.document.osu;
        Ok(inner.settings.clone())
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

    fn persist_ui_preferences(&self, settings: &AppSettings) -> Result<(), String> {
        let Some(store) = self.2.as_ref() else {
            return settings.save();
        };
        let before = store.snapshot().map_err(|error| error.to_string())?;
        let expected = SettingsPreferences::from_document(&before.document)?;
        let replacement = SettingsPreferences::from_document(settings)?;
        store
            .replace_preferences_if_unchanged(&expected, replacement)
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    fn publish_durable_settings(&self) -> Result<(), String> {
        let _save_guard = Self::lock_cloud_settings_save()?;
        let settings = self
            .0
            .lock()
            .map_err(|_| "runtime state lock poisoned")?
            .settings
            .clone();
        self.persist_ui_preferences(&settings)
    }

    fn publish_durable_settings_exclusive(
        &self,
        coordinator: &SettingsApplyCoordinator,
    ) -> Result<(), String> {
        coordinator
            .with_exclusive(|| self.publish_durable_settings())
            .map_err(|error| error.to_string())?
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
        let started = {
            let mut inner = self.0.lock().map_err(|_| "runtime state lock poisoned")?;
            if inner.tx.is_some() {
                return Ok(true);
            }
            inner.recording_desired = true;
            inner.last_save_request = None;
            if recorder_should_run(&inner.settings, inner.active_game.as_ref()) {
                let (tx, rx) = service::spawn(Self::options(&inner)?)?;
                let generation = Self::install_recording_sender(&mut inner, tx)?;
                Some((rx, generation))
            } else {
                None
            }
        };
        if let Some((rx, generation)) = started {
            if let Some(old_pump) = self.take_event_pump() {
                let _ = old_pump.join();
            }
            self.install_event_pump(pump_events(app, rx, generation));
        } else if let Some((generation, status)) = self.current_waiting_status() {
            let _ = app
                .state::<crate::desktop::tauri_sink::TauriUiEventSink>()
                .try_publish(UiEvent::Recorder {
                    generation: Generation::new(generation),
                    event: status,
                });
        }
        Ok(true)
    }

    fn stop_recording(&self) -> Result<bool, String> {
        let tx = {
            let mut inner = self.0.lock().map_err(|_| "runtime state lock poisoned")?;
            inner.recording_desired = false;
            inner.recording_generation =
                checked_generation_next(inner.recording_generation, "recording")?;
            let tx = inner.tx.take();
            inner.last_save_request = None;
            tx
        };
        if let Some(tx) = tx {
            let _ = tx.send(Cmd::Stop { announce: true });
        }
        Ok(false)
    }

    fn set_detected_game<R: Runtime>(
        &self,
        app: AppHandle<R>,
        detected: Option<DetectedGame>,
    ) -> Result<(), String> {
        let event = GameDetectionEvent::from_detected(detected.as_ref());
        let (prepared_restart, emit_event) = {
            let mut inner = self.0.lock().map_err(|_| "runtime state lock poisoned")?;
            record_osu_title_event(&mut inner, detected.as_ref(), unix_now_i64());
            if same_game_window(inner.active_game.as_ref(), detected.as_ref()) {
                if game_recording_mode_changed(inner.active_game.as_ref(), detected.as_ref()) {
                    inner.active_game = detected;
                    (Some(Self::prepare_service_restart(&mut inner)?), true)
                } else if inner.active_game != detected {
                    inner.active_game = detected;
                    (None, true)
                } else {
                    (None, false)
                }
            } else {
                inner.active_game = detected;
                (Some(Self::prepare_service_restart(&mut inner)?), true)
            }
        };
        if let Some(prepared) = prepared_restart {
            let waiting_for_game = prepared.waiting_for_game;
            let waiting_generation = prepared.waiting_generation;
            if let Some(tx) = prepared.old_tx {
                let _ = tx.send(Cmd::Stop { announce: false });
            }
            if let Some(old_pump) = self.take_event_pump() {
                let _ = old_pump.join();
            }
            if let Some((options, restart_generation)) = prepared.replacement {
                let (tx, rx) = service::spawn(options)?;
                let installed = {
                    let mut inner = self.0.lock().map_err(|_| "runtime state lock poisoned")?;
                    Self::install_prepared_service_restart(&mut inner, restart_generation, tx)
                };
                match installed {
                    Ok(generation) => {
                        self.install_event_pump(pump_events(app.clone(), rx, generation));
                    }
                    Err(tx) => {
                        let _ = tx.send(Cmd::Stop { announce: false });
                    }
                }
            }
            if let Some(generation) = waiting_generation.filter(|generation| {
                waiting_for_game && self.waiting_generation_is_current(*generation)
            }) {
                emit_waiting_for_game(&app, generation);
            }
        }
        if emit_event {
            publish_game_detection(&app, event)?;
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

enum AppUiActionResult {
    None,
    SaveQueued(bool),
    Recording(bool),
}

fn dispatch_ui_action<R: Runtime>(
    app: &AppHandle<R>,
    state: &RuntimeState,
    action: UiAction,
) -> Result<AppUiActionResult, String> {
    let effect = app
        .state::<crate::desktop::DesktopState>()
        .dispatch(action)?
        .effect;
    match effect {
        UiEffect::RequestSaveReplay => Ok(AppUiActionResult::SaveQueued(state.request_save())),
        UiEffect::SetRecording { recording } => {
            let recording = state.set_recording(app.clone(), recording)?;
            app.state::<crate::desktop::DesktopState>()
                .set_recorder_desired(recording)?;
            Ok(AppUiActionResult::Recording(recording))
        }
        UiEffect::SetLifecycle { mode } => {
            publish_window_lifecycle(app, mode);
            Ok(AppUiActionResult::None)
        }
        UiEffect::RequestSettingsProbe { token } => {
            if let Err(error) = app
                .state::<crate::settings_probe::SettingsProbeRuntime>()
                .submit(token, &state.settings())
            {
                let summary = clipline_desktop::ProbeSummary {
                    token,
                    phase: clipline_desktop::ProbePhase::Failed,
                    error: Some(error.clone()),
                };
                let _ = app
                    .state::<crate::desktop::tauri_sink::TauriUiEventSink>()
                    .try_publish(UiEvent::SettingsProbeChanged { summary });
                return Err(error);
            }
            Ok(AppUiActionResult::None)
        }
        UiEffect::None => Ok(AppUiActionResult::None),
    }
}

#[tauri::command]
fn save_replay<R: Runtime>(app: AppHandle<R>, state: tauri::State<RuntimeState>) {
    if let Ok(AppUiActionResult::SaveQueued(queued)) =
        dispatch_ui_action(&app, &state, UiAction::SaveReplay)
    {
        let _ = queued;
    }
}

#[tauri::command]
fn restart_as_administrator<R: Runtime>(app: AppHandle<R>) -> Result<bool, String> {
    if crate::windows::current_process_is_elevated()? {
        return Ok(false);
    }
    shutdown_app(&app, || {
        crate::windows::launch_elevated_after(std::process::id())
    })
    .map_err(|error| error.to_string())?;
    Ok(true)
}

#[tauri::command]
fn get_autostart_status(state: tauri::State<RuntimeState>) -> Result<bool, String> {
    autostart_status_for_build(
        state.settings().open_on_startup,
        cfg!(debug_assertions),
        || {
            autostart_registration()?
                .is_enabled()
                .map_err(|error| error.to_string())
        },
    )
}

fn autostart_registration() -> Result<WindowsAutostartRegistration, String> {
    let executable = std::env::current_exe()
        .map_err(|error| format!("resolve current executable for autostart: {error}"))?;
    WindowsAutostartRegistration::new(AUTOSTART_VALUE_NAME, &executable)
        .map_err(|error| error.to_string())
}

fn set_autostart(enabled: bool) -> Result<AutostartChange, String> {
    autostart_registration()?
        .set_enabled(enabled)
        .map_err(|error| error.to_string())
}

fn rollback_autostart(change: &AutostartChange) -> Result<(), String> {
    autostart_registration()?
        .rollback(change)
        .map_err(|error| error.to_string())
}

fn autostart_status_for_build(
    persisted_preference: bool,
    debug_build: bool,
    read_registry: impl FnOnce() -> Result<bool, String>,
) -> Result<bool, String> {
    if debug_build {
        Ok(persisted_preference)
    } else {
        read_registry()
    }
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
enum NativeWindowReconcileAction {
    None,
    BackgroundTaskbar,
    RestoreTaskbar,
}

fn close_request_effect(settings: &AppSettings) -> WindowEffect {
    if settings.close_to_tray {
        shared_window_effect(ShellWindowEvent::CloseRequested)
    } else {
        shared_window_effect(ShellWindowEvent::QuitRequested)
    }
}

fn minimize_request_effect(settings: &AppSettings) -> WindowEffect {
    if settings.minimize_to_tray {
        shared_window_effect(ShellWindowEvent::CloseRequested)
    } else {
        shared_window_effect(ShellWindowEvent::MinimizeRequested)
    }
}

fn shared_window_effect(event: ShellWindowEvent) -> WindowEffect {
    let (mut policy, _) = WindowPolicy::for_launch(LaunchMode::Normal);
    policy.apply(event)
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

fn send_main_window_to_tray<R: Runtime>(app: &AppHandle<R>) -> Result<(), String> {
    log_diagnostic(format!(
        "send main window to tray webviews={}",
        webview_labels(app)
    ));
    let mut windows = app
        .webview_windows()
        .into_iter()
        .filter(|(label, _)| is_app_window_label(label))
        .collect::<Vec<_>>();
    windows.sort_by(|a, b| a.0.cmp(&b.0));

    if windows.is_empty() {
        log_diagnostic("send-to-tray skipped: app window not found");
    }
    for (label, window) in windows {
        log_window_state(&format!("send-to-tray before label={label}"), &window);
        hide_main_window(
            || window.hide(),
            || publish_background_window(app, WindowLifecycleMode::Tray),
            || {
                let result = window.as_ref().hide();
                log_diagnostic(format!(
                    "send-to-tray webview hide label={label}: {}",
                    result_debug(result.as_ref())
                ));
                result
            },
            || request_webview_memory_target(&window, &label, crate::windows::MemoryTarget::Low),
        )?;
        log_diagnostic(format!("send-to-tray hide ok label={label}"));
        log_window_state(&format!("send-to-tray after hide label={label}"), &window);
    }
    Ok(())
}

fn publish_window_lifecycle<R: Runtime>(
    app: &AppHandle<R>,
    mode: WindowLifecycleMode,
) -> WindowLifecycleSnapshot {
    let snapshot = app.state::<WindowLifecycleState>().transition(mode);
    if let Err(error) = app
        .state::<crate::desktop::tauri_sink::TauriUiEventSink>()
        .try_publish(UiEvent::WindowLifecycle { snapshot })
    {
        log_diagnostic(format!(
            "window lifecycle publish failed revision={} mode={:?}: {error}",
            snapshot.revision, snapshot.mode
        ));
    }
    snapshot
}

fn publish_background_window<R: Runtime>(app: &AppHandle<R>, mode: WindowLifecycleMode) {
    app.state::<MicTestState>().stop();
    let background = publish_window_lifecycle(app, mode);
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_millis(250)).await;
        if app.state::<WindowLifecycleState>().snapshot() == background {
            crate::cloud::release_all_cloud_media_leases();
        }
    });
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

/// A cold autostart never calls `open_main_window`, and while the native window
/// is created hidden (`tauri.conf.json` `"visible": false`) wry initialises the
/// WebView2 controller from its `visible` attribute — so the webview would
/// render indefinitely in the background session that matters most. Hide it
/// explicitly; the reveal path turns it back on.
fn hide_autostart_webviews<R: Runtime>(app: &AppHandle<R>) {
    for (label, window) in app.webview_windows() {
        if !is_app_window_label(&label) {
            continue;
        }
        let result = window.as_ref().hide();
        log_diagnostic(format!(
            "autostart webview hide label={label}: {}",
            result_debug(result.as_ref())
        ));
        request_webview_memory_target(&window, &label, crate::windows::MemoryTarget::Low);
    }
}

#[derive(Debug, thiserror::Error)]
enum TauriShellError {
    #[error("enqueue {command:?} from {source}: {error}")]
    Enqueue {
        source: &'static str,
        command: ShellCommand,
        #[source]
        error: clipline_shell::ShellCommandSendError,
    },
    #[error("apply {command:?}: {message}")]
    Action {
        command: ShellCommand,
        message: String,
    },
    #[error("schedule {command:?} on the Tauri thread: {message}")]
    Schedule {
        command: ShellCommand,
        message: String,
    },
    #[error("advance shared shutdown contract: {0}")]
    ShutdownContract(#[from] clipline_shell::ShutdownError),
    #[error("acquire process shutdown ownership: {0}")]
    ShutdownOwnership(#[from] clipline_shell::ShutdownOwnershipError),
    #[error("quiesce update work for shutdown: {0}")]
    UpdateOperation(#[from] clipline_updater::UpdateOperationError),
}

fn enqueue_shell_command(
    sender: &ShellCommandSender,
    command: ShellCommand,
    source: &'static str,
) -> Result<(), TauriShellError> {
    sender
        .try_send(command)
        .map(|_| ())
        .map_err(|error| TauriShellError::Enqueue {
            source,
            command,
            error,
        })
}

fn report_shell_error<R: Runtime>(app: &AppHandle<R>, error: TauriShellError) {
    let message = format!("native shell: {error}");
    log_diagnostic(&message);
    tracing::error!(event = "native_shell_command_failed", error = %error);
    publish_user_error(app, message);
}

fn quit_app<R: Runtime>(app: &AppHandle<R>) -> Result<(), TauriShellError> {
    shutdown_app(app, || Ok(()))
}

fn shutdown_app<R: Runtime>(
    app: &AppHandle<R>,
    before_exit: impl FnOnce() -> Result<(), String>,
) -> Result<(), TauriShellError> {
    log_diagnostic("quit app requested");
    let shutdown_gate = app.state::<ShutdownGate>();
    let shutdown_owner = shutdown_gate.begin(ShutdownReason::Quit)?;
    let update_gate = app.state::<UpdateOperationGate>();
    let updates_quiesced = update_gate.quiesce_and_wait(UPDATE_QUIESCE_TIMEOUT)?;
    let mut uploads_quiesced =
        UploadQuiescence::begin(app).map_err(|message| TauriShellError::Action {
            command: ShellCommand::Quit,
            message,
        })?;
    let started = Instant::now();
    let mut shutdown = ShutdownCoordinator::new();
    let mut effect = shutdown.begin(
        ShutdownReason::Quit,
        0,
        u64::try_from(SHELL_SHUTDOWN_TIMEOUT.as_millis()).expect("shutdown timeout fits u64"),
    )?;

    loop {
        effect = match effect {
            ShutdownEffect::PublishDurableState { generation } => {
                app.state::<RuntimeState>()
                    .publish_durable_settings_exclusive(&app.state::<SettingsApplyCoordinator>())
                    .map_err(|message| TauriShellError::Action {
                        command: ShellCommand::Quit,
                        message,
                    })?;
                shutdown.acknowledge(
                    generation,
                    ShutdownAcknowledgement::DurableStatePublished,
                    elapsed_millis(started),
                )?
            }
            ShutdownEffect::StopWindowMedia { generation } => {
                app.state::<MicTestState>().stop();
                publish_window_lifecycle(app, WindowLifecycleMode::Tray);
                shutdown.acknowledge(
                    generation,
                    ShutdownAcknowledgement::WindowMediaStopped,
                    elapsed_millis(started),
                )?
            }
            ShutdownEffect::FinalizeRecorder { generation } => {
                app.state::<RuntimeState>()
                    .stop_recorder_and_wait(RECORDER_FINALIZATION_TIMEOUT)
                    .map_err(|message| TauriShellError::Action {
                        command: ShellCommand::Quit,
                        message,
                    })?;
                shutdown.acknowledge(
                    generation,
                    ShutdownAcknowledgement::RecorderFinalized,
                    elapsed_millis(started),
                )?
            }
            ShutdownEffect::FlushDiagnostics { generation } => {
                diagnostics::flush().map_err(|message| TauriShellError::Action {
                    command: ShellCommand::Quit,
                    message,
                })?;
                shutdown.acknowledge(
                    generation,
                    ShutdownAcknowledgement::DiagnosticsFlushed,
                    elapsed_millis(started),
                )?
            }
            ShutdownEffect::ReadyToExit {
                reason: ShutdownReason::Quit,
                ..
            } => {
                before_exit().map_err(|message| TauriShellError::Action {
                    command: ShellCommand::Quit,
                    message,
                })?;
                uploads_quiesced.commit_shutdown();
                shutdown_owner.commit_exit();
                updates_quiesced.commit_exit();
                app.exit(0);
                return Ok(());
            }
            ShutdownEffect::ReadyToExit {
                reason: ShutdownReason::InstallUpdate,
                ..
            } => {
                return Err(TauriShellError::Action {
                    command: ShellCommand::Quit,
                    message: "quit reached an update-install shutdown effect".into(),
                });
            }
        };
    }
}

fn block_on_isolated_runtime<F, T>(future: F) -> Result<T, String>
where
    F: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    std::thread::Builder::new()
        .name("clipline-upload-finalization".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .map_err(|error| format!("create upload finalization runtime: {error}"))?;
            Ok(runtime.block_on(future))
        })
        .map_err(|error| format!("spawn upload finalization thread: {error}"))?
        .join()
        .map_err(|_| "upload finalization thread panicked".to_string())?
}

struct UploadQuiescence {
    service: Option<UploadService>,
}

impl UploadQuiescence {
    fn begin<R: Runtime>(app: &AppHandle<R>) -> Result<Self, String> {
        let service = app
            .try_state::<crate::cloud_upload::TauriUploadState>()
            .map(|uploads| uploads.service().clone());
        let guard = Self { service };
        let Some(service) = guard.service.clone() else {
            return Ok(guard);
        };
        service.quiesce();
        block_on_isolated_runtime(async move {
            tokio::time::timeout(UPLOAD_FINALIZATION_TIMEOUT, service.wait_idle())
                .await
                .map_err(|_| "timed out finalizing active cloud uploads".to_string())
        })??;
        Ok(guard)
    }

    fn commit_shutdown(&mut self) {
        if let Some(service) = self.service.take() {
            service.shutdown();
        }
    }
}

impl Drop for UploadQuiescence {
    fn drop(&mut self) {
        if let Some(service) = self.service.take() {
            let _ = service.resume();
        }
    }
}

fn elapsed_millis(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
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
    match minimize_request_effect(&state.settings()) {
        WindowEffect::ShowInTaskbar => {
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
        WindowEffect::DropToTray => send_main_window_to_tray(&app),
        effect => Err(format!(
            "shared shell returned invalid minimize effect {effect:?}"
        )),
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
    if mode == WindowLifecycleMode::Tray {
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
    match dispatch_ui_action(&app, &state, UiAction::SetRecording { recording })? {
        AppUiActionResult::Recording(recording) => Ok(recording),
        AppUiActionResult::None | AppUiActionResult::SaveQueued(_) => {
            Err("recording action returned an incompatible result".to_owned())
        }
    }
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
) -> Result<(Option<UpdateManifest>, Option<String>), String> {
    if !channel_enabled(channel) {
        return Err(format!("{} updates are not available yet", channel.label()));
    }

    let current_version = semver::Version::parse(&app.package_info().version.to_string())
        .map_err(|error| format!("parse current application version: {error}"))?;
    let policy = UpdatePolicy::new(
        current_version,
        channel,
        UpdateVariant::from_standalone(is_standalone_install(app)),
    );
    match check_update(&policy)
        .await
        .map_err(|error| error.to_string())?
    {
        UpdateCheck::Available(update) => Ok((Some(update), None)),
        UpdateCheck::Current => Ok((None, None)),
        UpdateCheck::MissingRelease => Ok((None, Some(missing_release_metadata_message(channel)))),
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
    check_for_updates_inner(&app, &state).await
}

async fn check_for_updates_inner<R: Runtime>(
    app: &AppHandle<R>,
    state: &RuntimeState,
) -> Result<UpdateCheckResult, String> {
    let operations = app.state::<UpdateOperationGate>();
    let _operation = operations
        .begin(UpdateOperationKind::Check)
        .map_err(|error| error.to_string())?;
    let settings = state.settings();
    let channel = settings.update_channel;
    let current_version = app.package_info().version.to_string();
    let (update, status) = check_update_for_channel(app, channel).await?;

    Ok(UpdateCheckResult {
        channel,
        channel_label: channel.label(),
        current_version,
        available: update.is_some(),
        version: update.as_ref().map(|update| update.version.to_string()),
        date: update.as_ref().map(|update| update.pub_date.clone()),
        notes: update.as_ref().map(|update| update.notes.clone()),
        endpoint: channel.endpoint(is_standalone_install(app)),
        status,
    })
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct AppUpdateShutdownError(String);

struct TauriUpdateShutdown<'a, R: Runtime> {
    app: &'a AppHandle<R>,
    state: &'a RuntimeState,
    uploads: Option<UploadQuiescence>,
}

impl<R: Runtime> UpdateShutdown for TauriUpdateShutdown<'_, R> {
    type Error = AppUpdateShutdownError;

    fn publish_durable_state(&mut self) -> Result<(), Self::Error> {
        if self.uploads.is_none() {
            self.uploads = Some(UploadQuiescence::begin(self.app).map_err(AppUpdateShutdownError)?);
        }
        self.state
            .publish_durable_settings_exclusive(&self.app.state::<SettingsApplyCoordinator>())
            .map_err(AppUpdateShutdownError)
    }

    fn stop_window_media(&mut self) -> Result<(), Self::Error> {
        self.app.state::<MicTestState>().stop();
        publish_window_lifecycle(self.app, WindowLifecycleMode::Tray);
        Ok(())
    }

    fn stop_recorder(&mut self) -> Result<(), Self::Error> {
        self.state
            .stop_recorder_and_wait(Duration::from_secs(10))
            .map_err(AppUpdateShutdownError)
    }

    fn flush_diagnostics(&mut self) -> Result<(), Self::Error> {
        diagnostics::flush().map_err(AppUpdateShutdownError)
    }

    fn request_exit(&mut self) -> Result<(), Self::Error> {
        if let Some(uploads) = self.uploads.as_mut() {
            uploads.commit_shutdown();
        }
        self.app.exit(0);
        Ok(())
    }
}

fn update_download_destination(release_filename: &str) -> Result<PathBuf, String> {
    let directory = std::env::temp_dir()
        .join("Clipline")
        .join("update-downloads");
    std::fs::create_dir_all(&directory)
        .map_err(|error| format!("create update download directory: {error}"))?;
    Ok(directory.join(format!(
        "clipline-update-{}-{}-{release_filename}",
        std::process::id(),
        uuid::Uuid::new_v4()
    )))
}

fn cleanup_stale_update_downloads() {
    let directory = std::env::temp_dir()
        .join("Clipline")
        .join("update-downloads");
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten().take(64) {
        let path = entry.path();
        if path.is_file()
            && path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("clipline-update-"))
        {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[tauri::command]
async fn install_update<R: Runtime>(
    app: AppHandle<R>,
    state: tauri::State<'_, RuntimeState>,
) -> Result<(), String> {
    install_update_inner(&app, &state).await
}

async fn install_update_inner<R: Runtime>(
    app: &AppHandle<R>,
    state: &RuntimeState,
) -> Result<(), String> {
    let operations = app.state::<UpdateOperationGate>();
    let operation = operations
        .begin(UpdateOperationKind::Install)
        .map_err(|error| error.to_string())?;
    let channel = state.settings().update_channel;
    let (update, status) = check_update_for_channel(app, channel).await?;
    let Some(update) = update else {
        return Err(status.unwrap_or_else(|| "no update is available".into()));
    };

    let variant = UpdateVariant::from_standalone(is_standalone_install(app));
    let release_filename = installer_filename(&update.version, variant);
    let destination = update_download_destination(&release_filename)?;
    let telemetry = download_installer(
        update.target.url.clone(),
        &destination,
        operation.cancellation(),
    )
    .await
    .map_err(|error| error.to_string())?;
    let verified = verify_download(telemetry, &update.target.signature, &release_filename)
        .map_err(|error| error.to_string())?;
    let shutdown_gate = app.state::<ShutdownGate>();
    let shutdown_owner = shutdown_gate
        .begin(ShutdownReason::InstallUpdate)
        .map_err(|error| error.to_string())?;
    let mut launcher = WindowsInstallerLauncher;
    let mut shutdown = TauriUpdateShutdown {
        app,
        state,
        uploads: None,
    };
    let receipt = install_verified(&mut launcher, &mut shutdown, verified)
        .map_err(|error| error.to_string())?;
    shutdown_owner.commit_exit();
    operation.commit_exit();
    log_diagnostic(format!(
        "verified update handoff process_id={} bytes={} sha256={}",
        receipt.process_id(),
        receipt.telemetry().bytes_written,
        receipt.telemetry().sha256_hex()
    ));
    Ok(())
}

#[tauri::command]
fn get_settings(
    state: tauri::State<RuntimeState>,
    desktop: tauri::State<crate::desktop::DesktopState>,
) -> AppSettings {
    let settings = state.settings();
    if let Err(error) = desktop.replace_settings(settings.clone()) {
        log_diagnostic(format!(
            "desktop settings query reconciliation failed: {error}"
        ));
        return settings;
    }
    desktop.snapshot().settings
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

#[tauri::command(async)]
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

#[tauri::command(async)]
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

#[tauri::command(async)]
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
#[tauri::command(async)]
fn extract_window_icon(process_id: u32) -> Option<String> {
    let path = crate::games::list_game_windows()
        .into_iter()
        .find(|window| window.process_id == process_id)?
        .exe_path?;
    crate::game_icon::extract_exe_icon_data_url(&path)
}

#[tauri::command(async)]
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
    let ui_sink = app
        .state::<crate::desktop::tauri_sink::TauriUiEventSink>()
        .inner()
        .clone();
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
                        match clipline_desktop::MicMonitor::from_parts(
                            chunk.level.rms,
                            chunk.level.peak,
                            chunk.level.sample_count,
                            samples,
                        ) {
                            Ok(monitor) => {
                                let _ = ui_sink.try_publish(UiEvent::MicMonitor {
                                    generation: Generation::new(generation),
                                    monitor,
                                });
                            }
                            Err(error) => tracing::error!(
                                event = "microphone_monitor_payload_rejected",
                                error = %error
                            ),
                        }
                    });
                }
                Ok(())
            };
            if let Err(e) = run() {
                let mic_state = worker_app.state::<MicTestState>();
                mic_state.finish_if_active_with(generation, || {
                    let _ = ui_sink.try_publish(UiEvent::MicTestError {
                        generation: Generation::new(generation),
                        message: e,
                    });
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

fn configured_hotkeys(settings: &AppSettings) -> Result<HotkeySet, String> {
    HotkeySet::parse(&settings.hotkeys()).map_err(|error| error.to_string())
}

#[derive(Default)]
struct HotkeyServiceState {
    service: Mutex<Option<WindowsHotkeyService>>,
    dispatcher: Mutex<Option<std::thread::JoinHandle<()>>>,
    dispatcher_stop: std::sync::Arc<AtomicBool>,
}

impl HotkeyServiceState {
    fn install_service(&self, service: WindowsHotkeyService) -> Result<(), String> {
        let mut service_slot = self
            .service
            .lock()
            .map_err(|_| "hotkey service lock poisoned".to_string())?;
        if service_slot.is_some() {
            return Err("hotkey service is already installed".into());
        }
        *service_slot = Some(service);
        Ok(())
    }

    fn install_dispatcher(&self, dispatcher: std::thread::JoinHandle<()>) -> Result<(), String> {
        let mut dispatcher_slot = self
            .dispatcher
            .lock()
            .map_err(|_| "shell dispatcher lock poisoned".to_string())?;
        if dispatcher_slot.is_some() {
            return Err("shell dispatcher is already installed".into());
        }
        *dispatcher_slot = Some(dispatcher);
        Ok(())
    }

    fn replace_with_receipt(
        &self,
        candidate: &HotkeySet,
    ) -> Result<AppliedHotkeyReplacement, String> {
        self.service
            .lock()
            .map_err(|_| "hotkey service lock poisoned".to_string())?
            .as_ref()
            .ok_or_else(|| "hotkey service is unavailable".to_string())?
            .replace_with_receipt(candidate)
            .map_err(|error| error.to_string())
    }

    fn rollback(&self, receipt: HotkeyReplacementReceipt) -> Result<(), String> {
        self.service
            .lock()
            .map_err(|_| "hotkey service lock poisoned".to_string())?
            .as_ref()
            .ok_or_else(|| "hotkey service is unavailable".to_string())?
            .rollback(receipt)
            .map_err(|error| error.to_string())
    }
}

impl Drop for HotkeyServiceState {
    fn drop(&mut self) {
        if let Ok(service) = self.service.get_mut() {
            drop(service.take());
        }
        self.dispatcher_stop.store(true, Ordering::Release);
        if let Ok(dispatcher) = self.dispatcher.get_mut() {
            if let Some(dispatcher) = dispatcher.take() {
                let _ = dispatcher.join();
            }
        }
    }
}

fn spawn_shell_dispatch<R: Runtime>(
    app: AppHandle<R>,
    receiver: ShellCommandReceiver,
    stop: std::sync::Arc<AtomicBool>,
) -> Result<std::thread::JoinHandle<()>, String> {
    std::thread::Builder::new()
        .name("clipline-shell-dispatch".into())
        .spawn(move || {
            while !stop.load(Ordering::Acquire) {
                match receiver.wait_recv(Duration::from_millis(250)) {
                    Ok(Some(update)) => {
                        if let Err(error) = dispatch_shell_command(&app, update) {
                            report_shell_error(&app, error);
                        }
                    }
                    Ok(None) => {}
                    Err(_) => break,
                }
            }
        })
        .map_err(|error| format!("spawn hotkey command dispatcher: {error}"))
}

fn dispatch_shell_command<R: Runtime>(
    app: &AppHandle<R>,
    update: SequencedShellCommand,
) -> Result<(), TauriShellError> {
    match update.command {
        ShellCommand::Open => {
            let open_app = app.clone();
            app.run_on_main_thread(move || {
                if let Err(message) = open_main_window(&open_app) {
                    report_shell_error(
                        &open_app,
                        TauriShellError::Action {
                            command: ShellCommand::Open,
                            message,
                        },
                    );
                }
            })
            .map_err(|error| TauriShellError::Schedule {
                command: ShellCommand::Open,
                message: error.to_string(),
            })
        }
        ShellCommand::SaveReplay => {
            let state = app.state::<RuntimeState>();
            dispatch_ui_action(app, &state, UiAction::SaveReplay)
                .map(|_| ())
                .map_err(|message| TauriShellError::Action {
                    command: ShellCommand::SaveReplay,
                    message,
                })
        }
        ShellCommand::OpenDiagnostics => {
            support::open_diagnostics_folder().map_err(|message| TauriShellError::Action {
                command: ShellCommand::OpenDiagnostics,
                message,
            })
        }
        ShellCommand::Quit => quit_app(app),
        ShellCommand::CheckUpdates | ShellCommand::InstallUpdate => {
            let update_app = app.clone();
            tauri::async_runtime::spawn(async move {
                let state = update_app.state::<RuntimeState>();
                let result = match update.command {
                    ShellCommand::CheckUpdates => check_for_updates_inner(&update_app, &state)
                        .await
                        .map(|result| {
                            log_diagnostic(format!(
                                "native update check completed available={} version={:?}",
                                result.available, result.version
                            ));
                        }),
                    ShellCommand::InstallUpdate => install_update_inner(&update_app, &state).await,
                    _ => unreachable!("match arm restricts native update commands"),
                };
                if let Err(message) = result {
                    report_shell_error(
                        &update_app,
                        TauriShellError::Action {
                            command: update.command,
                            message,
                        },
                    );
                }
            });
            Ok(())
        }
    }
}

fn save_hotkey_label(settings: &AppSettings) -> String {
    settings.hotkeys().join(" / ")
}

struct PreparedSettingsPreflight {
    media_dir: PathBuf,
    quota_bytes: Option<u64>,
}

struct TauriSettingsApplyPorts<'a, R: Runtime> {
    app: &'a AppHandle<R>,
    runtime: &'a RuntimeState,
    tray: &'a TrayItems<R>,
    storage: &'a crate::library::StorageSettings,
    authorization: &'a NativeMediaFolderAuthorization,
}

#[cfg(test)]
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

impl<R: Runtime> SettingsApplyPorts for TauriSettingsApplyPorts<'_, R> {
    type PreparedPreflight = PreparedSettingsPreflight;
    type HotkeyReceipt = HotkeyReplacementReceipt;
    type TrayReceipt = TrayLabelReceipt;
    type AutostartReceipt = Option<AutostartChange>;
    type PreparedRecorder = PreparedRuntimeRestart;

    fn prepare_preflight(
        &mut self,
        baseline: &SettingsPreferences,
        candidate: &SettingsPreferences,
    ) -> Result<Self::PreparedPreflight, String> {
        let old_media_dir = PathBuf::from(&baseline.media_dir);
        let media_dir = PathBuf::from(&candidate.media_dir);
        self.authorization
            .validate_change(&old_media_dir, &media_dir)?;
        service::prepare_writable_media_directory(&media_dir)?;
        Ok(PreparedSettingsPreflight {
            media_dir,
            quota_bytes: quota_bytes_from_gb(candidate.disk_quota_gb)?,
        })
    }

    fn apply_hotkeys(
        &mut self,
        _baseline: &SettingsPreferences,
        candidate: &SettingsPreferences,
    ) -> Result<(Self::HotkeyReceipt, Vec<String>), String> {
        let mut labels = vec![candidate.hotkey.as_str()];
        if let Some(secondary) = candidate.hotkey_secondary.as_deref() {
            labels.push(secondary);
        }
        let hotkeys = HotkeySet::parse(&labels).map_err(|error| error.to_string())?;
        let applied = self
            .app
            .state::<HotkeyServiceState>()
            .replace_with_receipt(&hotkeys)?;
        Ok((applied.receipt, applied.outcome.warnings))
    }

    fn rollback_hotkeys(&mut self, receipt: Self::HotkeyReceipt) -> Result<(), String> {
        self.app.state::<HotkeyServiceState>().rollback(receipt)
    }

    fn apply_tray_label(
        &mut self,
        _baseline: &SettingsPreferences,
        candidate: &SettingsPreferences,
    ) -> Result<Self::TrayReceipt, String> {
        let mut labels = vec![candidate.hotkey.as_str()];
        if let Some(secondary) = candidate.hotkey_secondary.as_deref() {
            labels.push(secondary);
        }
        self.tray.replace_hotkey_label(&labels.join(" / "))
    }

    fn rollback_tray_label(&mut self, receipt: Self::TrayReceipt) -> Result<(), String> {
        self.tray.rollback_hotkey_label(receipt)
    }

    fn apply_autostart(
        &mut self,
        baseline: bool,
        requested: bool,
    ) -> Result<(Self::AutostartReceipt, bool), String> {
        let persisted = saved_autostart_preference_for_current_build(requested, baseline);
        if persisted == baseline || !autostart_should_mutate_for_current_build() {
            return Ok((None, persisted));
        }
        let change = set_autostart(persisted)
            .map_err(|error| format!("update Windows startup registration: {error}"))?;
        let enabled = change.enabled();
        Ok((Some(change), enabled))
    }

    fn rollback_autostart(&mut self, receipt: Self::AutostartReceipt) -> Result<(), String> {
        receipt.as_ref().map_or(Ok(()), rollback_autostart)
    }

    fn prepare_recorder(
        &mut self,
        candidate: &SettingsPreferences,
    ) -> Result<Self::PreparedRecorder, String> {
        let mut document = self.runtime.settings();
        candidate.apply_to_document(&mut document)?;
        self.runtime.prepare_settings_restart(document)
    }

    fn persist_preferences(
        &mut self,
        baseline: &SettingsPreferences,
        candidate: SettingsPreferences,
    ) -> Result<SettingsSnapshot, String> {
        self.runtime
            .2
            .as_ref()
            .ok_or_else(|| "settings apply requires a durable settings store".to_string())?
            .replace_preferences_if_unchanged(baseline, candidate)
            .map_err(|error| error.to_string())
    }

    fn commit_recorder(
        &mut self,
        prepared: Self::PreparedRecorder,
        authoritative: &SettingsSnapshot,
    ) {
        self.runtime.finish_prepared_restart(
            self.app.clone(),
            prepared,
            authoritative.document.clone(),
        );
    }

    fn commit_preflight(
        &mut self,
        prepared: Self::PreparedPreflight,
        _authoritative: &SettingsSnapshot,
    ) {
        self.storage
            .replace(prepared.quota_bytes, prepared.media_dir.clone());
        self.authorization.commit(&prepared.media_dir);
    }

    fn publish(&mut self, authoritative: &SettingsSnapshot) {
        self.app
            .state::<crate::desktop::DesktopState>()
            .replace_settings_authoritative(authoritative.document.clone());
    }
}

#[tauri::command]
async fn save_settings<R: Runtime>(
    app: AppHandle<R>,
    settings: AppSettings,
) -> Result<AppSettings, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<RuntimeState>();
        let coordinator = app.state::<SettingsApplyCoordinator>();
        let tray_items = app.state::<TrayItems<R>>();
        let storage_settings = app.state::<crate::library::StorageSettings>();
        let media_folder_authorization = app.state::<NativeMediaFolderAuthorization>();
        let baseline = SettingsPreferences::from_document(&state.settings())?;
        let candidate = SettingsPreferences::from_document(&settings)?;
        let mut ports = TauriSettingsApplyPorts {
            app: &app,
            runtime: &state,
            tray: &tray_items,
            storage: &storage_settings,
            authorization: &media_folder_authorization,
        };
        let success = coordinator
            .apply(&mut ports, baseline, candidate)
            .map_err(|error| error.to_string())?;
        for message in success.warnings {
            tracing::warn!(event = "settings_apply_warning", message = %message);
            publish_user_error(&app, message);
        }
        Ok(success.snapshot.document)
    })
    .await
    .map_err(|error| format!("settings apply worker failed: {error}"))?
}

pub fn run(
    instance_guard: WindowsInstanceGuard,
    shell_sender: ShellCommandSender,
    shell_receiver: ShellCommandReceiver,
    launch: ShellLaunch,
) {
    let _diagnostics_guard = diagnostics::init().ok();
    cleanup_stale_update_downloads();
    if let Err(error) = install_diagnostic_handler(|event| log_diagnostic(event.to_string())) {
        log_diagnostic(format!("capture diagnostic setup: {error}"));
    }
    let settings_store = SettingsStore::open(SettingsProfile::installed());
    let mut settings = settings_store
        .snapshot()
        .expect("new settings store lock is available")
        .document;
    let mut startup_warnings = settings_store.startup_warnings().to_vec();
    for warning in &startup_warnings {
        log_diagnostic(format!("settings recovery: {warning}"));
        tracing::warn!(event = "settings_recovery_warning", message = %warning);
    }
    let args = launch.application_arguments();
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
    let startup_hotkeys = configured_hotkeys(&settings)
        .unwrap_or_else(|_| HotkeySet::parse(&["Alt+F10"]).expect("default hotkey is valid"));
    let desktop_state =
        crate::desktop::DesktopState::new(settings.clone(), startup_warnings.clone())
            .expect("initialize bounded desktop snapshot");
    let (ui_event_sink, ui_event_receiver) =
        crate::desktop::tauri_sink::TauriUiEventSink::channel();
    let settings_probe_runtime =
        crate::settings_probe::SettingsProbeRuntime::new(ui_event_sink.clone())
            .expect("start bounded settings probe runtime");
    let launched_by_autostart = launch.mode() == LaunchMode::Autostart;
    let active_files = ActiveFileRegistry::new();
    let runtime_state = RuntimeState::with_store_and_registry(
        settings.clone(),
        lol_url,
        settings_store,
        active_files.clone(),
    );

    tauri::Builder::default()
        .manage(runtime_state)
        .manage(active_files)
        .manage(desktop_state)
        .manage(crate::desktop::ProducerGenerations::default())
        .manage(ui_event_sink)
        .manage(settings_probe_runtime)
        .manage(StartupWarnings::new(startup_warnings))
        .manage(WindowLifecycleState::default())
        .manage(shell_sender.clone())
        .manage(ShutdownGate::new())
        .manage(UpdateOperationGate::new())
        .manage(SettingsApplyCoordinator::default())
        .manage(HotkeyServiceState::default())
        .manage(MicTestState::default())
        .manage(support::SupportState::default())
        .manage(crate::memory::MemorySampler::default())
        .manage(NativeMediaFolderAuthorization::default())
        .manage(crate::library::StorageSettings::new(quota_bytes, media_dir))
        .invoke_handler(tauri::generate_handler![
            save_replay,
            restart_as_administrator,
            set_recording,
            get_settings,
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
            acknowledge_desktop_notice,
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
            crate::cloud::release_cloud_media_lease,
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
            crate::library::open_media_folder,
            crate::library::storage_status
        ])
        .setup(move |app| {
            let upload_state = crate::cloud_upload::TauriUploadState::build(
                app.handle().clone(),
                app.state::<ActiveFileRegistry>().inner().clone(),
            )?;
            app.manage(upload_state);
            crate::desktop::tauri_sink::spawn_event_pump(
                app.handle().clone(),
                ui_event_receiver,
            )?;
            configure_bundled_ffmpeg(app);
            let osu_app = app.handle().clone();
            let osu_media_root = media_dir_for_setup.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = crate::osu_api::retry_pending_enrichment(&osu_app, osu_media_root).await
                {
                    tracing::warn!(event = "startup_osu_enrichment_retry_failed", error = %e);
                }
            });
            let dispatcher_stop = app
                .state::<HotkeyServiceState>()
                .dispatcher_stop
                .clone();
            match spawn_shell_dispatch(app.handle().clone(), shell_receiver, dispatcher_stop) {
                Ok(dispatcher) => {
                    if let Err(error) = app
                        .state::<HotkeyServiceState>()
                        .install_dispatcher(dispatcher)
                    {
                        tracing::error!(event = "shell_dispatch_install_failed", error = %error);
                        publish_user_error(app.handle(), error);
                    }
                }
                Err(error) => {
                    tracing::error!(event = "shell_dispatch_start_failed", error = %error);
                    publish_user_error(app.handle(), error);
                }
            }
            match WindowsHotkeyService::start(shell_sender.clone()) {
                Ok(service) => {
                    for warning in service.startup_warnings() {
                        let message = format!("low-level save hotkey unavailable: {warning}");
                        tracing::warn!(event = "save_hook_install_failed", message = %message);
                        publish_user_error(app.handle(), message);
                    }
                    match service.replace(&startup_hotkeys) {
                        Ok(outcome) => {
                            for message in outcome.warnings {
                                tracing::warn!(event = "global_hotkey_registration_failed", message = %message);
                                publish_user_error(app.handle(), message);
                            }
                        }
                        Err(error) => {
                            let message = format!(
                                "global save hotkey unavailable; continuing without it: {error}"
                            );
                            tracing::warn!(event = "global_hotkey_registration_failed", message = %message);
                            publish_user_error(app.handle(), message);
                        }
                    }
                    if let Err(error) = app
                        .state::<HotkeyServiceState>()
                        .install_service(service)
                    {
                        tracing::error!(event = "hotkey_service_install_failed", error = %error);
                        publish_user_error(app.handle(), error);
                    }
                }
                Err(error) => {
                    let message = format!("save hotkey service unavailable: {error}");
                    tracing::warn!(event = "save_hotkey_service_start_failed", message = %message);
                    publish_user_error(app.handle(), message);
                }
            }
            if let Err(e) = crate::library::prune_audio_preview_cache_on_startup() {
                tracing::warn!(event = "audio_preview_startup_prune_failed", error = %e);
            }

            // Keep release builds in sync with the user's setting. Debug builds
            // share settings and registry state with installed builds, so cargo
            // runs must not disable or replace the installed autostart entry.
            if autostart_should_mutate_for_current_build() {
                if let Err(error) = set_autostart(settings.open_on_startup) {
                    let message = format!("synchronize Windows startup registration: {error}");
                    tracing::warn!(event = "autostart_sync_failed", message = %message);
                    publish_user_error(app.handle(), message);
                }
            }

            // When launched by the autostart registry entry, start in the tray
            // instead of flashing the main window.
            log_diagnostic(format!(
                "setup start launched_by_autostart={launched_by_autostart} webviews={}",
                webview_labels(app.handle())
            ));

            let initial_hotkey_label = save_hotkey_label(&settings);
            let save_item = MenuItem::with_id(
                app,
                "save",
                save_menu_text(&initial_hotkey_label),
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
                hotkey_label: Mutex::new(initial_hotkey_label),
            });
            TrayIconBuilder::with_id("clipline")
                .icon(tray_icon())
                .tooltip("Clipline — replay buffer")
                .menu(&menu)
                .on_menu_event(move |app, event| {
                    let mapping = match event.id().as_ref() {
                        "open" => Some(("tray open", ShellCommand::Open)),
                        "save" => Some(("tray save", ShellCommand::SaveReplay)),
                        "diagnostics" => {
                            Some(("tray diagnostics", ShellCommand::OpenDiagnostics))
                        }
                        "quit" => Some(("tray quit", ShellCommand::Quit)),
                        other => {
                            log_diagnostic(format!("tray menu event: unknown id={other}"));
                            None
                        }
                    };
                    if let Some((source, command)) = mapping {
                        log_diagnostic(format!("{source} requested"));
                        if let Err(error) = enqueue_shell_command(
                            &app.state::<ShellCommandSender>(),
                            command,
                            source,
                        ) {
                            report_shell_error(app, error);
                        }
                    }
                })
                .on_tray_icon_event(|tray, event| {
                    if !matches!(event, TrayIconEvent::Move { .. }) {
                        log_diagnostic(format!("tray icon event: {event:?}"));
                    }
                    if should_open_on_tray_event(&event) {
                        log_diagnostic("tray icon event requests open");
                        let app = tray.app_handle();
                        if let Err(error) = enqueue_shell_command(
                            &app.state::<ShellCommandSender>(),
                            ShellCommand::Open,
                            "tray left click",
                        ) {
                            report_shell_error(app, error);
                        }
                    }
                })
                .build(app)?;
            log_diagnostic(format!("tray build complete webviews={}", webview_labels(app.handle())));

            if let Err(e) = app
                .state::<RuntimeState>()
                .start_recording(app.handle().clone())
            {
                let message = format!("recorder startup failed: {e}");
                tracing::error!(event = "recorder_startup_failed", message = %message);
                publish_user_error(app.handle(), message);
            } else if let Err(error) = app
                .state::<crate::desktop::DesktopState>()
                .set_recorder_desired(true)
            {
                tracing::error!(event = "desktop_recorder_state_failed", error = %error);
            }
            spawn_game_detector(app.handle().clone());

            // The main window is created hidden by default so autostart launches
            // don't flash it. Show it for normal launches.
            if !launched_by_autostart {
                log_diagnostic("normal launch opening main window");
                if let Err(e) = open_main_window(app.handle()) {
                    log_diagnostic(format!("normal launch open failed: {e}"));
                    tracing::error!(event = "startup_window_show_failed", error = %e);
                }
            } else {
                log_diagnostic("autostart launch hiding webview");
                hide_autostart_webviews(app.handle());
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
                match close_request_effect(&app.state::<RuntimeState>().settings()) {
                    WindowEffect::DropToTray => {
                        log_diagnostic("close request action: tray");
                        if let Err(e) = send_main_window_to_tray(app) {
                            log_diagnostic(format!("close to tray failed: {e}"));
                            tracing::error!(event = "close_to_tray_failed", error = %e);
                        }
                    }
                    WindowEffect::Quit => {
                        log_diagnostic("close request action: quit");
                        if let Err(error) = enqueue_shell_command(
                            &app.state::<ShellCommandSender>(),
                            ShellCommand::Quit,
                            "window close",
                        ) {
                            report_shell_error(app, error);
                        }
                    }
                    effect => {
                        report_shell_error(
                            app,
                            TauriShellError::Action {
                                command: ShellCommand::Quit,
                                message: format!(
                                    "shared shell returned invalid close effect {effect:?}"
                                ),
                            },
                        );
                    }
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
                if let Some(uploads) =
                    app.try_state::<crate::cloud_upload::TauriUploadState>()
                {
                    uploads.shutdown();
                }
                app.state::<MicTestState>().stop();
                app.state::<RuntimeState>()
                    .send(Cmd::Stop { announce: false });
            }
            _ => {}
        });
    // Keep instance ownership and the activation listener alive for the entire application run.
    // Tauri state (including the shell dispatcher) is already gone before this bounded join.
    if let Err(error) = instance_guard.shutdown() {
        log_diagnostic(format!("single-instance listener shutdown failed: {error}"));
        tracing::error!(event = "single_instance_shutdown_failed", error = %error);
    }
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
                        let _ = app
                            .state::<crate::desktop::tauri_sink::TauriUiEventSink>()
                            .try_publish(UiEvent::UserError {
                                message: format!("game detection: {e}"),
                            });
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
    let main_window = app.get_webview_window(MAIN_WINDOW_LABEL);

    match main_window_open_target(main_window.is_some()) {
        MainWindowOpenTarget::ExistingMain => {
            let window = main_window.expect("target requires main window");
            log_window_state("open existing before reveal", &window);
            let result = reveal_logged_window(&window, "open existing");
            log_window_state("open existing after reveal", &window);
            probe_webview_after_reveal(&window, "open existing after reveal");
            arm_frontend_ready_watchdog();
            result
        }
        MainWindowOpenTarget::NewMain => {
            log_diagnostic("open_main_window rebuilding missing main window");
            let window = build_main_window(app, MAIN_WINDOW_LABEL)?;
            log_window_state("open rebuilt before reveal", &window);
            let result = reveal_logged_window(&window, "open rebuilt");
            log_window_state("open rebuilt after reveal", &window);
            probe_webview_after_reveal(&window, "open rebuilt after reveal");
            arm_frontend_ready_watchdog();
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
    WebviewWindowBuilder::from_config(app, &config)
        .map_err(|e| e.to_string())?
        .build()
        .map_err(|e| e.to_string())
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

fn pump_events<R: Runtime>(
    handle: AppHandle<R>,
    event_rx: service::RecorderEventStream,
    generation: u64,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        for event in event_rx {
            let accepted = match &event {
                Event::Status { recording, .. } => handle
                    .state::<RuntimeState>()
                    .accept_service_status(generation, *recording),
                _ => handle
                    .state::<RuntimeState>()
                    .service_generation_is_current(generation),
            };
            if !accepted {
                continue;
            }
            handle
                .state::<RuntimeState>()
                .observe_runtime_event(generation, &event);
            if let Event::MediaRootResolved { path, .. } = &event {
                let media_root = PathBuf::from(path);
                handle
                    .state::<crate::library::StorageSettings>()
                    .set_media_dir(media_root);
            }
            if let Err(error) = handle
                .state::<crate::desktop::tauri_sink::TauriUiEventSink>()
                .try_publish(UiEvent::Recorder {
                    generation: Generation::new(generation),
                    event: event.clone(),
                })
            {
                tracing::error!(event = "recorder_ui_event_publish_failed", error = %error);
            }
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
    })
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
    use clipline_test_utils::TestDir;
    use std::sync::Arc;

    fn durable_upload_record(
        local_clip_id: &str,
        upload_generation: u64,
        path: &str,
        upload_status: &str,
    ) -> CloudUploadRecord {
        CloudUploadRecord {
            local_clip_id: local_clip_id.into(),
            client_clip_id: Some(format!("client-{local_clip_id}")),
            upload_generation: Some(upload_generation),
            path: path.into(),
            remote_clip_id: None,
            remote_url: None,
            visibility: "private".into(),
            upload_status: upload_status.into(),
            error: None,
            updated_at_unix: upload_generation,
        }
    }

    #[test]
    fn quota_parser_converts_gib_to_bytes() {
        assert_eq!(parse_quota_gb("1").unwrap(), Some(1024 * 1024 * 1024));
        assert_eq!(parse_quota_gb("0.5").unwrap(), Some(512 * 1024 * 1024));
    }

    #[test]
    fn quota_parser_zero_disables_gc() {
        assert_eq!(parse_quota_gb("0").unwrap(), None);
    }

    #[test]
    fn quota_parser_rejects_negative_or_non_numeric_values() {
        assert!(parse_quota_gb("-1").is_err());
        assert!(parse_quota_gb("nope").is_err());
    }

    #[test]
    fn startup_warnings_are_delivered_once_after_frontend_readiness() {
        let warnings = StartupWarnings::new(vec!["settings recovered".into()]);

        assert_eq!(warnings.take(), vec!["settings recovered"]);
        assert!(warnings.take().is_empty());
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
    fn producer_generations_fail_closed_instead_of_wrapping() {
        assert_eq!(
            checked_generation_next(u64::MAX, "test").unwrap_err(),
            "test generation exhausted"
        );

        let mic = MicTestState::default();
        mic.0.lock().unwrap().last_generation = u64::MAX;
        assert_eq!(
            mic.begin().unwrap_err(),
            "microphone test generation exhausted"
        );
        assert!(mic.0.lock().unwrap().active.is_none());

        let state = RuntimeState::new(AppSettings::default(), None);
        state.0.lock().unwrap().recording_generation = u64::MAX;
        let (tx, rx) = mpsc::channel();
        let error = {
            let mut inner = state.0.lock().unwrap();
            RuntimeState::install_recording_sender(&mut inner, tx).unwrap_err()
        };
        assert_eq!(error, "recording generation exhausted");
        let inner = state.0.lock().unwrap();
        assert!(inner.tx.is_none());
        assert!(!inner.recording_desired);
        assert!(rx.try_recv().is_err());
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
    fn store_backed_cloud_update_commits_before_updating_the_runtime_mirror() {
        let dir = TestDir::new("clipline-app", "settings-store-cloud-update");
        let store = SettingsStore::open(SettingsProfile::isolated(dir.path()));
        let initial = store.snapshot().unwrap();
        let state = RuntimeState::with_store(initial.document.clone(), None, store.clone());

        let result = state
            .update_cloud(|cloud| cloud.host_url = "https://clips.example.com".into())
            .unwrap();
        let persisted = store.snapshot().unwrap();

        assert_eq!(result.cloud.host_url, "https://clips.example.com");
        assert_eq!(state.settings().cloud, persisted.document.cloud);
        assert_eq!(persisted.revision.get(), initial.revision.get() + 1);
    }

    #[test]
    fn cloud_record_compare_exchange_reconciles_exact_aliases_and_preserves_other_settings() {
        let dir = TestDir::new("clipline-app", "settings-store-cloud-record-alias-cas");
        let store = SettingsStore::open(SettingsProfile::isolated(dir.path()));
        let initial = store.snapshot().unwrap();
        let path = r"D:\Videos\Clipline\same.mp4";
        let legacy_a = durable_upload_record("legacy-a", 3, path, "failed");
        let legacy_b = durable_upload_record("legacy-b", 4, path, "failed");
        let unrelated = durable_upload_record(
            "unrelated",
            5,
            r"D:\Videos\Clipline\other.mp4",
            "uploaded_private",
        );
        let mut document = initial.document.clone();
        document.fps = 120;
        document.osu.client_id = Some("61835".into());
        document.cloud.host_url = "https://clips.example.com".into();
        document.cloud.connected_user_id = Some("user-a".into());
        document.cloud.credential_target = Some("credential-a".into());
        document
            .cloud
            .uploads
            .insert("legacy-a".into(), legacy_a.clone());
        document
            .cloud
            .uploads
            .insert("legacy-b".into(), legacy_b.clone());
        document
            .cloud
            .uploads
            .insert("unrelated".into(), unrelated.clone());
        let seeded = store.replace_document(&initial, document).unwrap();
        let state = RuntimeState::with_store(seeded.document.clone(), None, store.clone());
        let replacement = durable_upload_record("source-1", 9, path, "queued");

        let result = state
            .compare_exchange_cloud_records(CloudRecordCas {
                account: seeded.account.clone(),
                account_generation: seeded.account_generation,
                kind: CloudRecordCasKind::Admit {
                    upload_generation: 9,
                },
                expected: vec![
                    CloudRecordSlot {
                        key: "source-1".into(),
                        record: None,
                    },
                    CloudRecordSlot {
                        key: "legacy-a".into(),
                        record: Some(legacy_a),
                    },
                    CloudRecordSlot {
                        key: "legacy-b".into(),
                        record: Some(legacy_b),
                    },
                ],
                replacement: Some(CloudRecordSlot {
                    key: "source-1".into(),
                    record: Some(replacement.clone()),
                }),
            })
            .unwrap();
        let persisted = store.snapshot().unwrap();

        assert_eq!(persisted.document.cloud.uploads.len(), 2);
        assert_eq!(persisted.document.cloud.uploads["source-1"], replacement);
        assert_eq!(persisted.document.cloud.uploads["unrelated"], unrelated);
        assert_eq!(persisted.document.fps, seeded.document.fps);
        assert_eq!(persisted.document.osu, seeded.document.osu);
        assert_eq!(persisted.document.media_dir, seeded.document.media_dir);
        assert_eq!(persisted.revision.get(), seeded.revision.get() + 1);
        assert_eq!(result, persisted);
        assert_eq!(state.settings(), persisted.document);
    }

    #[test]
    fn independent_upload_cas_survives_unrelated_settings_revisions() {
        let dir = TestDir::new("clipline-app", "independent-cloud-record-cas");
        let store = SettingsStore::open(SettingsProfile::isolated(dir.path()));
        let initial = store.snapshot().unwrap();
        let first = durable_upload_record("local-a", 1, r"D:\Videos\Clipline\first.mp4", "queued");
        let second =
            durable_upload_record("local-b", 2, r"D:\Videos\Clipline\second.mp4", "queued");
        let mut document = initial.document.clone();
        document.cloud.host_url = "https://clips.example.com".into();
        document.cloud.connected_user_id = Some("user-a".into());
        document.cloud.credential_target = Some("credential-a".into());
        document
            .cloud
            .uploads
            .insert("local-a".into(), first.clone());
        document
            .cloud
            .uploads
            .insert("local-b".into(), second.clone());
        let seeded = store.replace_document(&initial, document).unwrap();
        let state = RuntimeState::with_store(seeded.document.clone(), None, store.clone());

        let mut first_uploading = first.clone();
        first_uploading.upload_status = "uploading".into();
        first_uploading.updated_at_unix += 1;
        state
            .compare_exchange_cloud_records(CloudRecordCas {
                account: seeded.account.clone(),
                account_generation: seeded.account_generation,
                kind: CloudRecordCasKind::Advance {
                    upload_generation: 1,
                },
                expected: vec![CloudRecordSlot {
                    key: "local-a".into(),
                    record: Some(first),
                }],
                replacement: Some(CloudRecordSlot {
                    key: "local-a".into(),
                    record: Some(first_uploading.clone()),
                }),
            })
            .unwrap();
        state
            .publish_durable_settings()
            .expect("an unrelated settings publication advances the global revision");

        let mut second_uploading = second.clone();
        second_uploading.upload_status = "uploading".into();
        second_uploading.updated_at_unix += 1;
        state
            .compare_exchange_cloud_records(CloudRecordCas {
                account: seeded.account,
                account_generation: seeded.account_generation,
                kind: CloudRecordCasKind::Advance {
                    upload_generation: 2,
                },
                expected: vec![CloudRecordSlot {
                    key: "local-b".into(),
                    record: Some(second),
                }],
                replacement: Some(CloudRecordSlot {
                    key: "local-b".into(),
                    record: Some(second_uploading.clone()),
                }),
            })
            .unwrap();

        let persisted = store.snapshot().unwrap();
        assert_eq!(persisted.document.cloud.uploads["local-a"], first_uploading);
        assert_eq!(
            persisted.document.cloud.uploads["local-b"],
            second_uploading
        );
        assert_eq!(state.settings(), persisted.document);
    }

    #[test]
    fn cloud_record_path_reconciliation_is_one_exact_transaction() {
        let dir = TestDir::new("clipline-app", "settings-store-cloud-rename-cas");
        let store = SettingsStore::open(SettingsProfile::isolated(dir.path()));
        let initial = store.snapshot().unwrap();
        let old_path = r"D:\Videos\Clipline\Session.mp4";
        let legacy = durable_upload_record("legacy", 3, old_path, "failed");
        let mut current =
            durable_upload_record("current", 4, r"d:/videos/clipline/session.MP4", "queued");
        current.updated_at_unix = legacy.updated_at_unix + 1;
        let unrelated = durable_upload_record(
            "unrelated",
            5,
            r"D:\Videos\Clipline\Other.mp4",
            "uploaded_private",
        );
        let mut document = initial.document.clone();
        document.fps = 120;
        document.osu.client_id = Some("61835".into());
        document.cloud.uploads.insert("legacy".into(), legacy);
        document
            .cloud
            .uploads
            .insert("current".into(), current.clone());
        document
            .cloud
            .uploads
            .insert("unrelated".into(), unrelated.clone());
        let seeded = store.replace_document(&initial, document).unwrap();
        let state = RuntimeState::with_store(seeded.document.clone(), None, store.clone());
        let new_path = r"D:\Videos\Clipline\Ranked win.mp4";

        assert!(state
            .reconcile_cloud_record_path(r"\\?\d:\videos\clipline\SESSION.mp4", new_path)
            .unwrap());

        let persisted = store.snapshot().unwrap();
        assert_eq!(persisted.revision.get(), seeded.revision.get() + 1);
        assert_eq!(persisted.document.cloud.uploads.len(), 2);
        assert!(!persisted.document.cloud.uploads.contains_key("legacy"));
        assert_eq!(persisted.document.cloud.uploads["current"].path, new_path);
        assert_eq!(persisted.document.cloud.uploads["unrelated"], unrelated);
        assert_eq!(persisted.document.fps, seeded.document.fps);
        assert_eq!(persisted.document.osu, seeded.document.osu);
        assert_eq!(persisted.document.media_dir, seeded.document.media_dir);
        assert_eq!(state.settings(), persisted.document);
    }

    #[test]
    fn unmatched_cloud_record_path_reconciliation_is_byte_identical() {
        let dir = TestDir::new("clipline-app", "settings-store-cloud-rename-no-match");
        let store = SettingsStore::open(SettingsProfile::isolated(dir.path()));
        let initial = store.snapshot().unwrap();
        let state = RuntimeState::with_store(initial.document.clone(), None, store.clone());
        state.publish_durable_settings().unwrap();
        let before = store.snapshot().unwrap();
        let primary_before = std::fs::read(store.profile().settings_path()).unwrap();
        let backup_path = store
            .profile()
            .settings_path()
            .with_file_name("settings.json.bak");
        let backup_before = std::fs::read(&backup_path).ok();

        assert!(!state
            .reconcile_cloud_record_path(
                r"D:\Videos\Clipline\Missing.mp4",
                r"D:\Videos\Clipline\Renamed.mp4",
            )
            .unwrap());

        assert_eq!(store.snapshot().unwrap(), before);
        assert_eq!(state.settings(), before.document);
        assert_eq!(
            std::fs::read(store.profile().settings_path()).unwrap(),
            primary_before
        );
        assert_eq!(std::fs::read(backup_path).ok(), backup_before);
    }

    #[test]
    fn cloud_record_compare_exchange_rejects_stale_record_and_account_byte_identically() {
        let dir = TestDir::new("clipline-app", "settings-store-cloud-record-stale-cas");
        let store = SettingsStore::open(SettingsProfile::isolated(dir.path()));
        let initial = store.snapshot().unwrap();
        let state = RuntimeState::with_store(initial.document.clone(), None, store.clone());
        state
            .update_cloud(|cloud| {
                cloud.host_url = "https://clips.example.com".into();
                cloud.connected_user_id = Some("user-a".into());
                cloud.credential_target = Some("credential-a".into());
            })
            .unwrap();
        let account_a = store.snapshot().unwrap();
        let queued = durable_upload_record("local-1", 1, r"D:\Videos\Clipline\clip.mp4", "queued");
        state
            .compare_exchange_cloud_records(CloudRecordCas {
                account: account_a.account.clone(),
                account_generation: account_a.account_generation,
                kind: CloudRecordCasKind::Admit {
                    upload_generation: 1,
                },
                expected: vec![CloudRecordSlot {
                    key: "local-1".into(),
                    record: None,
                }],
                replacement: Some(CloudRecordSlot {
                    key: "local-1".into(),
                    record: Some(queued.clone()),
                }),
            })
            .unwrap();
        let mut retrying = queued.clone();
        retrying.upload_status = "retrying".into();
        retrying.updated_at_unix = 2;
        let admitted = store.snapshot().unwrap();
        state
            .compare_exchange_cloud_records(CloudRecordCas {
                account: admitted.account.clone(),
                account_generation: admitted.account_generation,
                kind: CloudRecordCasKind::Advance {
                    upload_generation: 1,
                },
                expected: vec![CloudRecordSlot {
                    key: "local-1".into(),
                    record: Some(queued.clone()),
                }],
                replacement: Some(CloudRecordSlot {
                    key: "local-1".into(),
                    record: Some(retrying.clone()),
                }),
            })
            .unwrap();

        let before_stale_record = store.snapshot().unwrap();
        let mirror_before_stale_record = state.settings();
        let primary_before_stale_record = std::fs::read(store.profile().settings_path()).unwrap();
        let backup_path = dir.path().join("settings.json.bak");
        let backup_before_stale_record = std::fs::read(&backup_path).unwrap();
        let mut uploaded = queued.clone();
        uploaded.upload_status = "uploaded_private".into();
        uploaded.remote_clip_id = Some("remote-1".into());
        uploaded.updated_at_unix = 3;
        let error = state
            .compare_exchange_cloud_records(CloudRecordCas {
                account: before_stale_record.account.clone(),
                account_generation: before_stale_record.account_generation,
                kind: CloudRecordCasKind::Advance {
                    upload_generation: 1,
                },
                expected: vec![CloudRecordSlot {
                    key: "local-1".into(),
                    record: Some(queued),
                }],
                replacement: Some(CloudRecordSlot {
                    key: "local-1".into(),
                    record: Some(uploaded.clone()),
                }),
            })
            .unwrap_err();
        assert!(error.contains("record changed"), "{error}");
        assert_eq!(store.snapshot().unwrap(), before_stale_record);
        assert_eq!(state.settings(), mirror_before_stale_record);
        assert_eq!(
            std::fs::read(store.profile().settings_path()).unwrap(),
            primary_before_stale_record
        );
        assert_eq!(
            std::fs::read(&backup_path).unwrap(),
            backup_before_stale_record
        );

        state
            .update_cloud(|cloud| {
                cloud.connected_user_id = Some("user-b".into());
                cloud.credential_target = Some("credential-b".into());
            })
            .unwrap();
        let before_stale_account = store.snapshot().unwrap();
        let mirror_before_stale_account = state.settings();
        let primary_before_stale_account = std::fs::read(store.profile().settings_path()).unwrap();
        let backup_before_stale_account = std::fs::read(&backup_path).unwrap();
        let error = state
            .compare_exchange_cloud_records(CloudRecordCas {
                account: account_a.account,
                account_generation: account_a.account_generation,
                kind: CloudRecordCasKind::Advance {
                    upload_generation: 1,
                },
                expected: vec![CloudRecordSlot {
                    key: "local-1".into(),
                    record: Some(retrying),
                }],
                replacement: Some(CloudRecordSlot {
                    key: "local-1".into(),
                    record: Some(uploaded),
                }),
            })
            .unwrap_err();
        assert!(error.contains("account changed"), "{error}");
        assert_eq!(store.snapshot().unwrap(), before_stale_account);
        assert_eq!(state.settings(), mirror_before_stale_account);
        assert_eq!(
            std::fs::read(store.profile().settings_path()).unwrap(),
            primary_before_stale_account
        );
        assert_eq!(
            std::fs::read(&backup_path).unwrap(),
            backup_before_stale_account
        );
    }

    #[test]
    fn rejected_store_commit_leaves_the_runtime_mirror_unchanged() {
        let dir = TestDir::new("clipline-app", "settings-store-external-edit");
        let store = SettingsStore::open(SettingsProfile::isolated(dir.path()));
        let initial = store.snapshot().unwrap();
        let state = RuntimeState::with_store(initial.document.clone(), None, store.clone());
        initial
            .document
            .save_to(store.profile().settings_path())
            .unwrap();

        let error = state
            .update_cloud(|cloud| cloud.host_url = "https://clips.example.com".into())
            .unwrap_err();

        assert!(error.contains("changed outside this process"), "{error}");
        assert_eq!(state.settings(), initial.document);
        assert_eq!(store.snapshot().unwrap(), initial);
    }

    #[test]
    fn durable_publish_advances_the_shared_store_snapshot() {
        let dir = TestDir::new("clipline-app", "durable-settings-publish");
        let store = SettingsStore::open(SettingsProfile::isolated(dir.path()));
        let initial = store.snapshot().unwrap();
        let state = RuntimeState::with_store(initial.document.clone(), None, store.clone());

        state.publish_durable_settings().unwrap();

        let committed = store.snapshot().unwrap();
        assert_eq!(committed.document, initial.document);
        assert_eq!(committed.revision.get(), initial.revision.get() + 1);
        assert!(store.profile().settings_path().is_file());
    }

    #[test]
    fn rejected_durable_publish_preserves_external_file_backup_store_and_runtime() {
        let dir = TestDir::new("clipline-app", "durable-settings-external-edit");
        let store = SettingsStore::open(SettingsProfile::isolated(dir.path()));
        let initial = store.snapshot().unwrap();
        let state = RuntimeState::with_store(initial.document.clone(), None, store.clone());
        state.publish_durable_settings().unwrap();
        state.publish_durable_settings().unwrap();
        let before = store.snapshot().unwrap();
        let primary_path = store.profile().settings_path();
        let backup_path = primary_path.with_file_name("settings.json.bak");
        let mut external_primary = std::fs::read(primary_path).unwrap();
        external_primary.extend_from_slice(b"\n");
        std::fs::write(primary_path, &external_primary).unwrap();
        let backup = std::fs::read(&backup_path).unwrap();

        let error = state.publish_durable_settings().unwrap_err();

        assert!(error.contains("changed outside this process"), "{error}");
        assert_eq!(std::fs::read(primary_path).unwrap(), external_primary);
        assert_eq!(std::fs::read(backup_path).unwrap(), backup);
        assert_eq!(store.snapshot().unwrap(), before);
        assert_eq!(state.settings(), before.document);
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
    fn updater_stop_waits_for_matching_recorder_finalization_acknowledgement() {
        let (tx, rx) = mpsc::channel();
        let state = Arc::new(RuntimeState::with_sender(tx, AppSettings::default(), None));
        let service_state = Arc::clone(&state);
        let service = std::thread::spawn(move || {
            assert!(matches!(
                rx.recv_timeout(Duration::from_secs(1)),
                Ok(Cmd::Stop { announce: true })
            ));
            let generation = service_state.0.lock().unwrap().recording_generation;
            assert!(service_state.accept_service_status(generation, false));
        });

        state
            .stop_recorder_and_wait(Duration::from_secs(1))
            .expect("matching stopped status acknowledges finalization");
        service.join().unwrap();
        assert!(state.0.lock().unwrap().tx.is_none());
    }

    #[test]
    fn updater_stop_timeout_fails_closed_without_exiting_or_losing_sender() {
        let (tx, rx) = mpsc::channel();
        let state = RuntimeState::with_sender(tx, AppSettings::default(), None);

        let error = state
            .stop_recorder_and_wait(Duration::from_millis(1))
            .expect_err("missing finalization acknowledgement must time out");

        assert!(error.contains("timed out"), "{error}");
        assert!(matches!(rx.try_recv(), Ok(Cmd::Stop { announce: true })));
        assert!(state.0.lock().unwrap().tx.is_some());
    }

    #[test]
    fn updater_stop_rejects_a_recorder_finalization_error_acknowledgement() {
        let (tx, rx) = mpsc::channel();
        let state = Arc::new(RuntimeState::with_sender(tx, AppSettings::default(), None));
        let service_state = Arc::clone(&state);
        let service = std::thread::spawn(move || {
            assert!(matches!(
                rx.recv_timeout(Duration::from_secs(1)),
                Ok(Cmd::Stop { announce: true })
            ));
            let generation = service_state.0.lock().unwrap().recording_generation;
            service_state.observe_runtime_event(
                generation,
                &Event::Error {
                    message: "finish failed".to_string(),
                },
            );
            assert!(service_state.accept_service_status(generation, false));
        });

        let error = state
            .stop_recorder_and_wait(Duration::from_secs(1))
            .expect_err("failed finalization must abort the update handoff");

        service.join().unwrap();
        assert!(error.contains("finalization failure"), "{error}");
        assert!(state.0.lock().unwrap().tx.is_none());
    }

    #[test]
    fn isolated_runtime_wait_is_safe_inside_a_tokio_runtime() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();

        let value = runtime.block_on(async {
            block_on_isolated_runtime(async { 42_u8 })
                .expect("dedicated wait thread must not nest block_on in the caller runtime")
        });

        assert_eq!(value, 42);
    }

    #[test]
    fn stale_stopped_status_does_not_clear_newer_recording_sender() {
        let (old_tx, _old_rx) = mpsc::channel();
        let state = RuntimeState::with_sender(old_tx, AppSettings::default(), None);
        let stale_generation = state.0.lock().unwrap().recording_generation;
        let (new_tx, new_rx) = mpsc::channel();
        {
            let mut inner = state.0.lock().unwrap();
            RuntimeState::install_recording_sender(&mut inner, new_tx).unwrap();
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
            Some((
                _,
                Event::Status {
                    recording: false,
                    waiting_for_game: true,
                    ..
                }
            ))
        ));
    }

    #[test]
    fn active_recorder_status_is_available_for_authoritative_bootstrap() {
        let (tx, _rx) = mpsc::channel();
        let state = RuntimeState::with_sender(tx, AppSettings::default(), None);
        let generation = state.0.lock().unwrap().recording_generation;
        state.observe_runtime_event(
            generation,
            &Event::Status {
                recording: true,
                waiting_for_game: false,
                segments: 4,
                buffered_s: 5.0,
                buffered_mb: 6.0,
                full_session: false,
                encoder: "H.264".into(),
                capture_backend: "wgc".into(),
            },
        );

        assert!(matches!(
            state.current_recorder_status(),
            Some((
                observed_generation,
                Event::Status {
                    recording: true,
                    segments: 4,
                    ..
                }
            )) if observed_generation == generation
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

    fn detected_game(id: &str, name: &str, hwnd: isize) -> DetectedGame {
        DetectedGame {
            identity: crate::game_identity::GameIdentity::custom(id),
            name: name.into(),
            hwnd,
            window_title: format!("{name} Window"),
            process_id: hwnd as u32,
            exe_name: format!("{name}.exe"),
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
    fn game_restart_pauses_service_but_keeps_recorder_armed() {
        let (tx, _rx) = mpsc::channel();
        let mut settings = AppSettings::default();
        settings.games.pause_when_no_game = true;
        let mut inner = RuntimeInner {
            tx: Some(tx),
            recording_generation: 1,
            recording_desired: true,
            settings,
            active_files: ActiveFileRegistry::new(),
            lol_url: None,
            active_game: None,
            osu_title_events: Vec::new(),
            last_save_request: Some(Instant::now()),
            decodable_codecs: vec![service::Codec::H264],
            last_recorder_status: None,
            last_storage_status: None,
            recent_recorder_error: false,
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
            settings,
            active_files: ActiveFileRegistry::new(),
            lol_url: None,
            active_game: Some(detected_game("custom-game", "Game", 42)),
            osu_title_events: Vec::new(),
            last_save_request: None,
            decodable_codecs: vec![service::Codec::H264],
            last_recorder_status: None,
            last_storage_status: None,
            recent_recorder_error: false,
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
            RuntimeState::install_recording_sender(&mut inner, newer_tx).unwrap();
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
            RuntimeState::install_recording_sender(&mut inner, started_tx).unwrap();
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
            options: None,
            worker: None,
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
            settings: invalid_disk_replay_settings(),
            active_files: ActiveFileRegistry::new(),
            lol_url: None,
            active_game: None,
            osu_title_events: Vec::new(),
            last_save_request: Some(Instant::now()),
            decodable_codecs: vec![service::Codec::H264],
            last_recorder_status: None,
            last_storage_status: None,
            recent_recorder_error: false,
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
            settings: AppSettings::default(),
            active_files: ActiveFileRegistry::new(),
            lol_url: None,
            active_game: None,
            osu_title_events: Vec::new(),
            last_save_request: Some(Instant::now()),
            decodable_codecs: vec![service::Codec::H264],
            last_recorder_status: None,
            last_storage_status: None,
            recent_recorder_error: false,
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
            RuntimeState::install_recording_sender(&mut inner, newer_tx).unwrap();
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
    fn settings_restart_gap_rejects_a_parked_sender_after_a_newer_start() {
        let state = RuntimeState::new(AppSettings::default(), None);
        let reserved_generation = {
            let mut inner = state.0.lock().unwrap();
            inner.recording_desired = true;
            inner.recording_generation = 7;
            7
        };

        let (newer_tx, newer_rx) = mpsc::channel();
        let (stale_tx, stale_rx) = mpsc::channel();
        let rejected = {
            let mut inner = state.0.lock().unwrap();
            RuntimeState::install_recording_sender(&mut inner, newer_tx).unwrap();
            RuntimeState::install_settings_restart_sender(&mut inner, reserved_generation, stale_tx)
                .unwrap_err()
        };
        rejected.send(Cmd::Stop { announce: false }).unwrap();

        assert!(state.send(Cmd::Save));
        assert!(matches!(newer_rx.try_recv(), Ok(Cmd::Save)));
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
            settings: invalid_disk_replay_settings(),
            active_files: ActiveFileRegistry::new(),
            lol_url: None,
            active_game: Some(DetectedGame {
                identity: crate::game_identity::GameIdentity::custom("custom-game"),
                name: "Game".into(),
                hwnd: 42,
                window_title: "Game".into(),
                process_id: 7,
                exe_name: "game.exe".into(),
                recording_mode: GameRecordingMode::FullSession,
            }),
            osu_title_events: Vec::new(),
            last_save_request: Some(Instant::now()),
            decodable_codecs: vec![service::Codec::H264],
            last_recorder_status: None,
            last_storage_status: None,
            recent_recorder_error: false,
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
            settings: invalid_disk_replay_settings(),
            active_files: ActiveFileRegistry::new(),
            lol_url: None,
            active_game: None,
            osu_title_events: Vec::new(),
            last_save_request: None,
            decodable_codecs: vec![service::Codec::H264],
            last_recorder_status: None,
            last_storage_status: None,
            recent_recorder_error: false,
        };

        assert!(RuntimeState::prepare_service_restart(&mut inner).is_err());
        assert_eq!(inner.recording_generation, 8);
        assert!(inner.recording_desired);
        assert!(inner.tx.is_none());
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
            settings: AppSettings::default(),
            active_files: ActiveFileRegistry::new(),
            lol_url: None,
            active_game: None,
            osu_title_events: Vec::new(),
            last_save_request: None,
            decodable_codecs: vec![service::Codec::H264],
            last_recorder_status: None,
            last_storage_status: None,
            recent_recorder_error: false,
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
        assert_eq!(close_request_effect(&defaults), WindowEffect::DropToTray);
        assert_eq!(
            minimize_request_effect(&defaults),
            WindowEffect::ShowInTaskbar
        );

        let settings = AppSettings {
            close_to_tray: false,
            minimize_to_tray: true,
            ..AppSettings::default()
        };
        assert_eq!(close_request_effect(&settings), WindowEffect::Quit);
        assert_eq!(minimize_request_effect(&settings), WindowEffect::DropToTray);
    }

    #[test]
    fn debug_build_autostart_policy_skips_registry_mutation() {
        assert!(!autostart_should_mutate_for_build(true));
        assert!(autostart_should_mutate_for_build(false));
    }

    #[test]
    fn debug_autostart_status_returns_persisted_preference_without_registry_access() {
        assert!(autostart_status_for_build(true, true, || {
            panic!("debug status must not read the shared installed Run value")
        })
        .unwrap());
        assert!(!autostart_status_for_build(false, true, || {
            panic!("benchmark status must not read the shared installed Run value")
        })
        .unwrap());
        assert!(autostart_status_for_build(false, false, || Ok(true)).unwrap());
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
        let main = include_str!("main.rs");
        let acquire = main
            .find("acquire_or_activate(")
            .expect("native instance ownership should be acquired");
        let app_run = main
            .find("app::run(instance, shell_sender, shell_receiver, launch)")
            .expect("primary instance should construct the app");
        assert!(
            acquire < app_run,
            "instance ownership must be established before app construction"
        );

        let app = include_str!("app.rs");
        let run_start = app.find("pub fn run(").expect("run function should exist");
        let run_body = &app[run_start..];
        let run_end = run_body
            .find("\nfn spawn_game_detector")
            .expect("run function should be followed by spawn_game_detector");
        let run_body = &run_body[..run_end];
        let setup = run_body
            .find(".setup(move |app|")
            .expect("app setup should be registered");
        let recorder_start = run_body
            .find("start_recording(app.handle().clone())")
            .expect("setup should start the recorder after plugins are installed");

        assert!(
            setup < recorder_start,
            "initial recorder startup must happen only from setup"
        );
        assert!(
            !main[..acquire].contains("app::run(") && !main[..acquire].contains("service::spawn("),
            "startup must not construct the app or recorder before duplicate rejection"
        );
    }

    #[test]
    fn native_shell_enqueue_failure_is_typed_before_application_work() {
        let (sender, receiver) = clipline_shell::shell_command_channel();
        drop(receiver);

        let error = enqueue_shell_command(&sender, ShellCommand::Quit, "test")
            .expect_err("a disconnected command port must fail closed");
        assert!(matches!(
            error,
            TauriShellError::Enqueue {
                source: "test",
                command: ShellCommand::Quit,
                error: clipline_shell::ShellCommandSendError::Disconnected,
            }
        ));
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
            WindowLifecycleSnapshot::new(Revision::new(0), WindowLifecycleMode::Tray)
        );
        assert_eq!(
            state.transition(WindowLifecycleMode::Tray),
            WindowLifecycleSnapshot::new(Revision::new(0), WindowLifecycleMode::Tray)
        );
        assert_eq!(
            state.transition(WindowLifecycleMode::Foreground),
            WindowLifecycleSnapshot::new(Revision::new(1), WindowLifecycleMode::Foreground)
        );
        assert_eq!(
            state.transition(WindowLifecycleMode::Taskbar),
            WindowLifecycleSnapshot::new(Revision::new(2), WindowLifecycleMode::Taskbar)
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
            settings: AppSettings::default(),
            active_files: ActiveFileRegistry::new(),
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
                recording_mode: GameRecordingMode::FullSession,
            }),
            osu_title_events: Vec::new(),
            last_save_request: None,
            decodable_codecs: vec![service::Codec::H264],
            last_recorder_status: None,
            last_storage_status: None,
            recent_recorder_error: false,
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
            settings: AppSettings::default(),
            active_files: ActiveFileRegistry::new(),
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
                recording_mode: GameRecordingMode::FullSession,
            }),
            osu_title_events: Vec::new(),
            last_save_request: None,
            decodable_codecs: vec![service::Codec::H264],
            last_recorder_status: None,
            last_storage_status: None,
            recent_recorder_error: false,
        };

        let opts = RuntimeState::options(&inner).unwrap();

        assert_eq!(
            opts.active_game
                .as_ref()
                .and_then(|game| game.identity.plugin_id()),
            Some(crate::game_plugins::LEAGUE_OF_LEGENDS_ID)
        );
        assert_eq!(opts.lol_url.as_deref(), Some("http://mock"));
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
