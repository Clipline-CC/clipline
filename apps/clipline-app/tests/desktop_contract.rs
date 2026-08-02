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
