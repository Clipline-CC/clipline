use std::fs;
use std::path::PathBuf;

use clipline_desktop::{
    CloudUploadProgress, DesktopController, Generation, RecorderEvent, UiEvent, MAX_ACTIVE_UPLOADS,
};
use clipline_slint_spike::desktop::{DesktopProjection, RevisionGate};

fn spike_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn upload(generation: u64, id: &str) -> UiEvent {
    UiEvent::CloudUploadProgress {
        generation: Generation::new(generation),
        progress: CloudUploadProgress {
            local_clip_id: id.into(),
            path: format!(r"C:\{id}.mp4"),
            upload_status: "uploading".into(),
            received_size_bytes: generation,
            file_size_bytes: 100,
            remote_clip_id: None,
            remote_url: None,
            error: None,
        },
    }
}

#[test]
fn projection_is_revisioned_and_bounds_the_visible_upload_model() {
    let mut controller = DesktopController::new((), vec!["warning".into()]).unwrap();
    controller
        .apply_event(UiEvent::Recorder {
            generation: Generation::new(1),
            event: RecorderEvent::Status {
                recording: true,
                waiting_for_game: false,
                segments: 2,
                buffered_s: 3.0,
                buffered_mb: 4.0,
                full_session: false,
                encoder: "H.264".into(),
                capture_backend: "wgc".into(),
            },
        })
        .unwrap();
    for index in 0..MAX_ACTIVE_UPLOADS {
        controller
            .apply_event(upload(index as u64 + 1, &format!("clip-{index:02}")))
            .unwrap();
    }

    let snapshot = controller.snapshot();
    let projection = DesktopProjection::from_snapshot(&snapshot);
    assert_eq!(projection.revision, snapshot.revision);
    assert_eq!(projection.recorder_label, "RECORDING · H.264");
    assert_eq!(projection.notice, "warning");
    assert_eq!(projection.uploads.len(), MAX_ACTIVE_UPLOADS);
    assert_eq!(projection.uploads[0].local_clip_id, "clip-00");
}

#[test]
fn delayed_projection_is_rejected_after_a_newer_revision_is_posted() {
    let gate = RevisionGate::default();
    gate.post(4);
    assert!(gate.is_current(4));
    gate.post(5);
    assert!(!gate.is_current(4));
    assert!(gate.is_current(5));
}

#[test]
fn adapter_source_posts_only_weak_revision_gated_ui_closures() {
    let source = fs::read_to_string(spike_root().join("src/desktop.rs")).unwrap();
    for required in [
        "ui_event_channel()",
        "thread::Builder::new()",
        "slint::invoke_from_event_loop",
        "gate.is_current(revision)",
        "weak.upgrade()",
        "slint::ModelRc::new",
        "consumer_alive.store(false",
    ] {
        assert!(
            source.contains(required),
            "missing adapter contract: {required}"
        );
    }
    assert!(!source.contains("tauri"));
    assert!(!source.contains("webview"));
}
