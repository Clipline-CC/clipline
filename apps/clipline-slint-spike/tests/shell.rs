use clipline_desktop::Revision;
use clipline_shell::{LaunchMode, ShellCommand};
use clipline_slint_spike::shell::{
    LibraryRefreshCursor, LifecycleAction, ShellLabels, ShellLifecycle, WindowTeardownStep,
    WINDOW_TEARDOWN_ORDER,
};

fn create_window(shell: &mut ShellLifecycle) -> clipline_slint_spike::shell::AttachmentToken {
    let LifecycleAction::CreateWindow { attachment } =
        shell.handle_command(ShellCommand::Open).unwrap()
    else {
        panic!("an absent window must be created");
    };
    shell.window_created(attachment).unwrap();
    attachment
}

#[test]
fn autostart_is_tray_only_until_the_first_open() {
    let (mut shell, initial) = ShellLifecycle::for_launch(LaunchMode::Autostart).unwrap();
    assert_eq!(initial, LifecycleAction::KeepTrayOnly);
    assert!(!shell.snapshot().window_active);
    assert_eq!(shell.snapshot().counters.windows_created, 0);
    assert_eq!(shell.snapshot().counters.windows_dropped, 0);

    let attachment = create_window(&mut shell);
    assert!(shell.accept_callback(attachment).unwrap());
    assert_eq!(shell.snapshot().counters.windows_created, 1);
}

#[test]
fn normal_and_repeated_open_create_at_most_one_window() {
    let (mut shell, initial) = ShellLifecycle::for_launch(LaunchMode::Normal).unwrap();
    let LifecycleAction::CreateWindow { attachment } = initial else {
        panic!("normal launch creates the first window");
    };
    shell.window_created(attachment).unwrap();

    assert_eq!(
        shell.handle_command(ShellCommand::Open).unwrap(),
        LifecycleAction::RevealWindow { attachment }
    );
    assert_eq!(shell.snapshot().counters.windows_created, 1);
    assert_eq!(shell.snapshot().counters.open_requests, 2);
}

#[test]
fn close_to_tray_has_one_pinned_resource_release_order() {
    assert_eq!(
        WINDOW_TEARDOWN_ORDER,
        [
            WindowTeardownStep::PublishBackground,
            WindowTeardownStep::StopWindowMedia,
            WindowTeardownStep::StopPlayback,
            WindowTeardownStep::ReleasePresentationResources,
            WindowTeardownStep::DetachDesktop,
            WindowTeardownStep::DropComponent,
        ]
    );

    let (mut shell, _) = ShellLifecycle::for_launch(LaunchMode::Autostart).unwrap();
    let attachment = create_window(&mut shell);
    assert_eq!(
        shell.close_requested(attachment).unwrap(),
        LifecycleAction::DropToTray { attachment }
    );
    shell.window_dropped(attachment).unwrap();
    assert!(!shell.snapshot().window_active);
    assert_eq!(shell.snapshot().counters.windows_dropped, 1);
}

#[test]
fn reattach_invalidates_every_old_window_callback() {
    let (mut shell, _) = ShellLifecycle::for_launch(LaunchMode::Autostart).unwrap();
    let first = create_window(&mut shell);
    shell.close_requested(first).unwrap();
    shell.window_dropped(first).unwrap();
    let second = create_window(&mut shell);

    assert!(!shell.accept_callback(first).unwrap());
    assert!(shell.accept_callback(second).unwrap());
    assert_eq!(shell.snapshot().counters.stale_callbacks, 1);
}

#[test]
fn one_hundred_cycles_have_exact_counts_and_no_active_window() {
    let (mut shell, _) = ShellLifecycle::for_launch(LaunchMode::Autostart).unwrap();
    for _ in 0..100 {
        let attachment = create_window(&mut shell);
        shell.close_requested(attachment).unwrap();
        shell.window_dropped(attachment).unwrap();
    }

    let snapshot = shell.snapshot();
    assert!(!snapshot.window_active);
    assert_eq!(snapshot.counters.windows_created, 100);
    assert_eq!(snapshot.counters.windows_dropped, 100);
    assert_eq!(snapshot.counters.open_requests, 100);
    assert_eq!(snapshot.counters.close_requests, 100);
}

#[test]
fn failed_window_creation_returns_to_tray_and_open_can_retry() {
    let (mut shell, _) = ShellLifecycle::for_launch(LaunchMode::Autostart).unwrap();
    let LifecycleAction::CreateWindow { attachment: first } =
        shell.handle_command(ShellCommand::Open).unwrap()
    else {
        panic!("first Open must start window creation");
    };
    shell.window_create_failed(first).unwrap();

    let LifecycleAction::CreateWindow { attachment: second } =
        shell.handle_command(ShellCommand::Open).unwrap()
    else {
        panic!("Open must remain usable after a failed window factory");
    };
    assert_ne!(first, second);
    shell.window_created(second).unwrap();
    assert_eq!(shell.snapshot().counters.windows_created, 1);
}

#[test]
fn quit_is_idempotent_and_tray_window_labels_are_explicitly_synchronized() {
    let labels = ShellLabels::new("RECORDING · H.264", "Alt+F10  Save Replay");
    let (mut shell, _) = ShellLifecycle::for_launch_with_labels(LaunchMode::Autostart, labels)
        .expect("valid shell lifecycle");
    assert_eq!(shell.tray_labels(), shell.window_labels());

    let attachment = create_window(&mut shell);
    assert_eq!(
        shell.handle_command(ShellCommand::Quit).unwrap(),
        LifecycleAction::Quit {
            attachment: Some(attachment)
        }
    );
    assert_eq!(
        shell.handle_command(ShellCommand::Quit).unwrap(),
        LifecycleAction::None
    );
    assert_eq!(shell.snapshot().counters.quit_effects, 1);
}

#[test]
fn save_and_diagnostics_preserve_the_durable_shell_actions() {
    let (mut shell, _) = ShellLifecycle::for_launch(LaunchMode::Autostart).unwrap();
    assert_eq!(
        shell.handle_command(ShellCommand::SaveReplay).unwrap(),
        LifecycleAction::SaveReplay
    );
    assert_eq!(
        shell.handle_command(ShellCommand::OpenDiagnostics).unwrap(),
        LifecycleAction::OpenDiagnostics
    );
}

#[test]
fn library_refresh_cursor_coalesces_bursts_and_rejects_replays() {
    let mut cursor = LibraryRefreshCursor::new(Revision::INITIAL);
    assert!(!cursor.observe_attached(Revision::INITIAL, true));
    assert!(cursor.observe_attached(Revision::new(3), true));
    assert_eq!(cursor.observed(), Revision::new(3));
    assert!(!cursor.observe_attached(Revision::new(2), true));
    assert!(!cursor.observe_attached(Revision::new(3), true));
    assert!(cursor.observe_attached(Revision::new(4), true));
}

#[test]
fn tray_only_library_revision_remains_pending_for_the_next_attachment() {
    let mut cursor = LibraryRefreshCursor::new(Revision::INITIAL);
    assert!(!cursor.observe_attached(Revision::new(7), false));
    assert_eq!(cursor.observed(), Revision::INITIAL);
    assert!(cursor.observe_attached(Revision::new(7), true));
    assert_eq!(cursor.observed(), Revision::new(7));
}
