use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use tauri::{
    AppHandle, Manager, Runtime, WebviewWindow, WebviewWindowBuilder,
};


use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WindowLifecycleMode {
    Foreground,
    Taskbar,
    /// Close-to-tray has requested destroy; the label may still be registered.
    Destroying,
    /// No live app UI. Opens may build a fresh main window.
    Destroyed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub(crate) struct WindowLifecycleSnapshot {
    pub(crate) revision: u64,
    pub(crate) mode: WindowLifecycleMode,
    pub(crate) backgrounded: bool,
}

impl WindowLifecycleSnapshot {
    pub(crate) fn new(revision: u64, mode: WindowLifecycleMode) -> Self {
        Self {
            revision,
            mode,
            // Destroying/Destroyed are backgrounded: the UI is gone or going away,
            // so async frontend work must not recreate gallery/media.
            backgrounded: mode != WindowLifecycleMode::Foreground,
        }
    }
}

pub(crate) struct WindowLifecycleState(pub(crate) Mutex<WindowLifecycleSnapshot>);

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
    pub(crate) fn snapshot(&self) -> WindowLifecycleSnapshot {
        match self.0.lock() {
            Ok(snapshot) => *snapshot,
            Err(poisoned) => *poisoned.into_inner(),
        }
    }

    pub(crate) fn transition(&self, mode: WindowLifecycleMode) -> WindowLifecycleSnapshot {
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
pub(crate) struct MainWindowOpenQueue(pub(crate) AtomicBool);

impl Default for MainWindowOpenQueue {
    fn default() -> Self {
        Self(AtomicBool::new(false))
    }
}

impl MainWindowOpenQueue {
    pub(crate) fn pending(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    pub(crate) fn set_pending(&self, pending: bool) {
        self.0.store(pending, Ordering::Release);
    }
}

/// Per-window UI readiness. Generation 0 means no live main window.
pub(crate) struct FrontendReadinessState {
    /// Monotonic allocator; never resets across destroy/recreate.
    pub(crate) next_generation: AtomicU64,
    /// Currently live UI generation; 0 means no live main window.
    pub(crate) generation: AtomicU64,
    pub(crate) ready_generation: AtomicU64,
    pub(crate) watchdog_armed_generation: AtomicU64,
    pub(crate) destroy_started: Mutex<Option<Instant>>,
}

#[derive(Clone, Copy)]
pub(crate) struct FrontendReadinessCheckpoint {
    pub(crate) generation: u64,
    pub(crate) ready_generation: u64,
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
    pub(crate) fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    pub(crate) fn ready_generation(&self) -> u64 {
        self.ready_generation.load(Ordering::Acquire)
    }

    pub(crate) fn begin_generation(&self) -> u64 {
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

    pub(crate) fn clear_for_destroy(&self) -> FrontendReadinessCheckpoint {
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

    pub(crate) fn restore_after_failed_destroy(&self, checkpoint: FrontendReadinessCheckpoint) {
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

    pub(crate) fn mark_ready(&self) -> Option<u64> {
        let generation = self.generation();
        if generation == 0 {
            return None;
        }
        self.ready_generation.store(generation, Ordering::Release);
        Some(generation)
    }

    pub(crate) fn is_ready(&self, generation: u64) -> bool {
        generation != 0 && self.generation() == generation && self.ready_generation() == generation
    }

    /// Returns true when this call newly arms the watchdog for the generation.
    pub(crate) fn try_arm_watchdog(&self, generation: u64) -> bool {
        if generation == 0 || self.is_ready(generation) {
            return false;
        }
        let previous = self
            .watchdog_armed_generation
            .swap(generation, Ordering::AcqRel);
        previous != generation
    }
}

pub(crate) fn watchdog_should_fire(
    armed_generation: u64,
    current_generation: u64,
    ready_generation: u64,
) -> bool {
    armed_generation != 0
        && armed_generation == current_generation
        && ready_generation != armed_generation
}

pub(crate) fn ensure_foreground_microphone_test(state: &WindowLifecycleState) -> Result<(), String> {
    if state.snapshot().mode == WindowLifecycleMode::Foreground {
        Ok(())
    } else {
        Err("microphone test is unavailable while Clipline is backgrounded".into())
    }
}

#[derive(serde::Serialize)]
pub(crate) struct FrontendReadyResponse {
    pub(crate) warnings: Vec<String>,
    pub(crate) window_lifecycle: WindowLifecycleSnapshot,
}

pub(crate) fn open_main_window<R: Runtime>(app: &AppHandle<R>) -> Result<(), String> {
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

pub(crate) fn build_main_window<R: Runtime>(
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

pub(crate) fn reveal_logged_window<R: Runtime>(
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
pub(crate) fn reveal_main_window<E>(
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
pub(crate) fn hide_main_window<E>(
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn destroying_and_destroyed_modes_are_backgrounded() {
        assert!(WindowLifecycleSnapshot::new(1, WindowLifecycleMode::Destroying).backgrounded);
        assert!(WindowLifecycleSnapshot::new(2, WindowLifecycleMode::Destroyed).backgrounded);
        assert!(!WindowLifecycleSnapshot::new(3, WindowLifecycleMode::Foreground).backgrounded);
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
}
