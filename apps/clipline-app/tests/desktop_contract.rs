use std::fs;
use std::path::PathBuf;

fn app_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn neutral_desktop_contract_is_a_first_class_dependency() {
    let cargo = fs::read_to_string(app_root().join("Cargo.toml")).unwrap();
    let main = fs::read_to_string(app_root().join("src/main.rs")).unwrap();
    assert!(cargo.contains("clipline-desktop"));
    assert!(main.contains("mod desktop;"));
}

#[test]
fn tauri_event_names_live_in_one_adapter() {
    let sink = fs::read_to_string(app_root().join("src/desktop/tauri_sink.rs")).unwrap();
    for name in [
        "status",
        "saved",
        "error",
        "window-lifecycle",
        "mic-test",
        "mic-test-error",
        "mic-test-stopped",
        "game-detection",
        "cloud-upload-progress",
        "osu-enrichment-updated",
    ] {
        assert!(sink.contains(name), "missing adapter event {name}");
    }
}

#[test]
fn recorder_entry_points_use_typed_actions_and_shared_events() {
    let app = fs::read_to_string(app_root().join("src/app.rs")).unwrap();
    let service = fs::read_to_string(app_root().join("src/service.rs")).unwrap();
    assert!(app.contains("dispatch_ui_action(&app, &state, UiAction::SaveReplay)"));
    assert!(app.contains("UiAction::SetRecording { recording }"));
    assert!(app.contains("UiEvent::Recorder"));
    assert!(service.contains("pub use clipline_desktop::RecorderEvent as Event;"));
    assert!(!service.contains("pub enum Event"));
}

#[test]
fn migrated_producers_cannot_emit_directly_to_tauri() {
    for relative in ["src/app.rs", "src/cloud.rs", "src/osu_api.rs"] {
        let source = fs::read_to_string(app_root().join(relative)).unwrap();
        assert!(
            !source.contains(".emit("),
            "{relative} bypasses the bounded desktop event adapter"
        );
    }
}

#[test]
fn frontend_bootstrap_is_versioned_sequenced_and_authoritative() {
    let app = fs::read_to_string(app_root().join("src/app.rs")).unwrap();
    let desktop = fs::read_to_string(app_root().join("src/desktop.rs")).unwrap();
    assert!(app.contains("desktop_snapshot: bootstrap.snapshot"));
    assert!(app.contains("desktop_event_sequence: bootstrap.event_sequence"));
    assert!(app.contains("desktop.snapshot().settings"));
    assert!(desktop.contains("pub fn apply_sequenced("));
    assert!(desktop.contains("pub fn bootstrap(&self) -> DesktopBootstrap"));
}

#[test]
fn shipping_shell_uses_one_bounded_launch_and_the_shared_command_port() {
    let main = fs::read_to_string(app_root().join("src/main.rs")).unwrap();
    let app = fs::read_to_string(app_root().join("src/app.rs")).unwrap();

    assert_eq!(
        main.matches("ShellLaunch::parse(").count(),
        1,
        "the process boundary must parse the bounded shared launch exactly once"
    );
    assert!(
        main.contains("wait_for_elevation_parent(launch.elevation_parent())")
            && main.contains("if let Some(parent) = launch.updater_parent()")
            && main.contains("wait_for_process_exit(parent)")
            && main.contains("launch.mode()")
            && main.contains("app::run(instance, shell_sender, shell_receiver, launch)"),
        "the parsed launch must drive parent handoff, activation, and the Tauri adapter"
    );
    assert!(
        !app.contains("std::env::args()")
            && app.contains("launch.application_arguments()")
            && app.contains("launch.mode()")
            && app.contains("WindowPolicy::for_launch(LaunchMode::Normal)"),
        "the Tauri adapter must not reinterpret raw launch or lifecycle decisions"
    );

    for command in [
        "ShellCommand::Open",
        "ShellCommand::SaveReplay",
        "ShellCommand::OpenDiagnostics",
        "ShellCommand::Quit",
        "ShellCommand::CheckUpdates",
        "ShellCommand::InstallUpdate",
    ] {
        assert!(
            app.contains(command),
            "the shipping shell must retain shared command route {command}"
        );
    }
    assert!(
        app.contains("fn enqueue_shell_command")
            && app.contains("fn dispatch_shell_command")
            && app.contains("publish_user_error")
            && !app.contains("has no native Tauri-shell ingress"),
        "native shell failures must be typed, logged, and visible through the existing UI route"
    );
    assert!(
        app.contains(".manage(ShutdownGate::new())")
            && app.contains(".manage(UpdateOperationGate::new())")
            && app.contains(".begin(UpdateOperationKind::Check)")
            && app.contains(".begin(UpdateOperationKind::Install)")
            && app.contains("operation.cancellation()")
            && app.contains("quiesce_and_wait(UPDATE_QUIESCE_TIMEOUT)"),
        "quit and updates must share one shutdown owner and a cancellable update gate"
    );
}
