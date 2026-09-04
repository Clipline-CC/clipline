use std::sync::atomic::Ordering;

use tauri::tray::{MouseButton, MouseButtonState, TrayIconEvent};
use tauri::{
    AppHandle, Manager, Runtime, WebviewWindow,
};


use super::*;

/// Destroy-aware open decision for the tray shell. Queued opens during
/// `Destroying` are expressible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MainWindowOpenAction {
    RevealExisting,
    QueueOpen,
    BuildNew,
    Noop,
}

/// Pure tray-shell state used to pin the destroy -> open race without a live
/// Tauri runtime. Production close-to-tray/open paths drive these helpers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MainWindowShellState {
    pub(crate) mode: WindowLifecycleMode,
    pub(crate) main_window_present: bool,
    pub(crate) pending_open: bool,
}

/// Enter `Destroying` while the dying label may still be registered.
pub(crate) fn begin_close_to_tray_destroy(state: &mut MainWindowShellState) {
    state.mode = WindowLifecycleMode::Destroying;
}

/// Queue while `Destroying`; build only when destroyed/absent; never reveal a
/// dying label.
pub(crate) fn request_main_window_open(state: &mut MainWindowShellState) -> MainWindowOpenAction {
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
pub(crate) fn observe_main_window_destroyed(state: &mut MainWindowShellState) -> MainWindowOpenAction {
    state.mode = WindowLifecycleMode::Destroyed;
    state.main_window_present = false;
    if state.pending_open {
        state.pending_open = false;
        MainWindowOpenAction::BuildNew
    } else {
        MainWindowOpenAction::Noop
    }
}

pub(crate) fn main_window_shell_state<R: Runtime>(app: &AppHandle<R>) -> MainWindowShellState {
    MainWindowShellState {
        mode: app.state::<WindowLifecycleState>().snapshot().mode,
        main_window_present: app.get_webview_window(MAIN_WINDOW_LABEL).is_some(),
        pending_open: app.state::<MainWindowOpenQueue>().pending(),
    }
}

pub(crate) fn persist_main_window_shell_pending<R: Runtime>(app: &AppHandle<R>, state: &MainWindowShellState) {
    app.state::<MainWindowOpenQueue>()
        .set_pending(state.pending_open);
}

pub(crate) fn prepare_frontend_readiness_for_destroy<R: Runtime>(
    app: &AppHandle<R>,
) -> FrontendReadinessCheckpoint {
    let checkpoint = app.state::<FrontendReadinessState>().clear_for_destroy();
    WEBVIEW_REPAIR_NOTICE_SHOWN.store(false, Ordering::Release);
    checkpoint
}

pub(crate) fn window_state_summary<R: Runtime>(window: &WebviewWindow<R>) -> String {
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

pub(crate) fn log_window_state<R: Runtime>(context: &str, window: &WebviewWindow<R>) {
    log_diagnostic(format!("{context}: {}", window_state_summary(window)));
}

pub(crate) fn should_open_on_tray_event(event: &TrayIconEvent) -> bool {
    match event {
        TrayIconEvent::Click {
            button,
            button_state,
            ..
        } => should_open_on_tray_click(*button, *button_state),
        _ => false,
    }
}

pub(crate) fn should_open_on_tray_click(button: MouseButton, button_state: MouseButtonState) -> bool {
    button == MouseButton::Left && button_state == MouseButtonState::Up
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{WindowLifecycleMode, is_app_window_label};

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
    fn close_to_tray_destroy_enters_destroying_until_destroyed_event() {
        let mut state = MainWindowShellState {
            mode: WindowLifecycleMode::Foreground,
            main_window_present: true,
            pending_open: false,
        };

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
        let mut state = MainWindowShellState {
            mode: WindowLifecycleMode::Destroying,
            main_window_present: true,
            pending_open: false,
        };

        let action = request_main_window_open(&mut state);

        assert_eq!(action, MainWindowOpenAction::QueueOpen);
        assert!(state.pending_open);
        assert_eq!(state.mode, WindowLifecycleMode::Destroying);
        assert!(state.main_window_present);
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
    fn destroying_or_destroyed_label_is_never_revealed() {
        let mut state = MainWindowShellState {
            mode: WindowLifecycleMode::Destroying,
            main_window_present: true,
            pending_open: false,
        };
        assert_eq!(
            request_main_window_open(&mut state),
            MainWindowOpenAction::QueueOpen
        );
        let mut state = MainWindowShellState {
            mode: WindowLifecycleMode::Destroyed,
            main_window_present: true,
            pending_open: false,
        };
        assert_eq!(
            request_main_window_open(&mut state),
            MainWindowOpenAction::BuildNew
        );
        let mut state = MainWindowShellState {
            mode: WindowLifecycleMode::Foreground,
            main_window_present: true,
            pending_open: false,
        };
        assert_eq!(
            request_main_window_open(&mut state),
            MainWindowOpenAction::RevealExisting
        );
    }

    #[test]
    fn immediate_close_then_open_race_builds_only_after_destroyed() {
        let mut state = MainWindowShellState {
            mode: WindowLifecycleMode::Foreground,
            main_window_present: true,
            pending_open: false,
        };
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
}
