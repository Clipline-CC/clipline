
use tauri::{
    AppHandle, Emitter, Manager, Runtime, WebviewWindow,
};


use crate::service::Cmd;
use crate::settings::AppSettings;
use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CloseRequestAction {
    Tray,
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MinimizeRequestAction {
    Taskbar,
    Tray,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NativeWindowReconcileAction {
    None,
    BackgroundTaskbar,
    RestoreTaskbar,
}

pub(crate) fn close_request_action(settings: &AppSettings) -> CloseRequestAction {
    if settings.close_to_tray {
        CloseRequestAction::Tray
    } else {
        CloseRequestAction::Quit
    }
}

pub(crate) fn minimize_request_action(settings: &AppSettings) -> MinimizeRequestAction {
    if settings.minimize_to_tray {
        MinimizeRequestAction::Tray
    } else {
        MinimizeRequestAction::Taskbar
    }
}

pub(crate) fn native_window_reconcile_action(
    mode: WindowLifecycleMode,
    is_minimized: bool,
) -> NativeWindowReconcileAction {
    match (mode, is_minimized) {
        (WindowLifecycleMode::Foreground, true) => NativeWindowReconcileAction::BackgroundTaskbar,
        (WindowLifecycleMode::Taskbar, false) => NativeWindowReconcileAction::RestoreTaskbar,
        _ => NativeWindowReconcileAction::None,
    }
}

pub(crate) fn send_main_window_to_tray<R: Runtime>(app: &AppHandle<R>) -> Result<(), String> {
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

pub(crate) fn complete_main_window_destroyed<R: Runtime>(app: &AppHandle<R>) -> Result<(), String> {
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

    resume_hotkeys_after_ui_gone(app);

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

pub(crate) fn publish_window_lifecycle<R: Runtime>(
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

pub(crate) fn publish_background_window<R: Runtime>(app: &AppHandle<R>, mode: WindowLifecycleMode) {
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
pub(crate) fn request_webview_memory_target<R: Runtime>(
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

pub(crate) fn quit_app<R: Runtime>(app: &AppHandle<R>) {
    log_diagnostic("quit app requested");
    app.state::<MicTestState>().stop();
    app.state::<crate::library::ClipboardExportState>().cancel();
    app.state::<RuntimeState>()
        .send(Cmd::Stop { announce: false });
    app.exit(0);
}

#[tauri::command]
pub(crate) fn minimize_main_window<R: Runtime>(
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

pub(crate) fn restore_taskbar_window<R: Runtime>(
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

pub(crate) fn restore_taskbar_webview<E>(
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

pub(crate) fn background_if_native_minimized<E>(
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

pub(crate) fn background_native_minimized_window<R: Runtime>(
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

pub(crate) fn reconcile_native_window<R: Runtime>(
    app: &AppHandle<R>,
    window: &WebviewWindow<R>,
) -> Result<(), String> {
    let mode = app.state::<WindowLifecycleState>().snapshot().mode;
    if matches!(
        mode,
        WindowLifecycleMode::Destroying | WindowLifecycleMode::Destroyed
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
pub(crate) fn set_recording<R: Runtime>(
    app: AppHandle<R>,
    state: tauri::State<RuntimeState>,
    recording: bool,
) -> Result<bool, String> {
    state.set_recording(app, recording)
}

#[tauri::command]
pub(crate) fn set_session_recording<R: Runtime>(
    app: AppHandle<R>,
    state: tauri::State<RuntimeState>,
    recording: bool,
) -> Result<bool, String> {
    state.set_session_recording(app, recording)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{WindowLifecycleMode, should_reconcile_native_window_event};
    use tauri::WindowEvent;

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
    fn native_window_reconciliation_ignores_stable_states() {
        assert_eq!(
            native_window_reconcile_action(WindowLifecycleMode::Foreground, false),
            NativeWindowReconcileAction::None
        );
        assert_eq!(
            native_window_reconcile_action(WindowLifecycleMode::Taskbar, true),
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
}
