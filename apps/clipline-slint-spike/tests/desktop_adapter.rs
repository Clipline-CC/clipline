use std::fs;
use std::path::PathBuf;

use clipline_desktop::{
    CloudUploadProgress, DesktopController, Generation, RecorderEvent, UiEvent, MAX_ACTIVE_UPLOADS,
};
use clipline_slint_spike::desktop::{AttachmentGate, DesktopProjection};

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
fn attachment_and_revision_both_fence_delayed_projection() {
    let gate = AttachmentGate::default();
    let first = gate.attach(4).unwrap();
    assert!(gate.is_current(first, 4));
    gate.post_revision(5);
    assert!(!gate.is_current(first, 4));
    assert!(gate.is_current(first, 5));

    gate.detach(first).unwrap();
    assert!(!gate.is_current(first, 5));
    let second = gate.attach(5).unwrap();
    assert_ne!(first, second);
    assert!(!gate.is_current(first, 5));
    assert!(gate.is_current(second, 5));
}

#[test]
fn stale_detach_cannot_invalidate_a_replacement_attachment() {
    let gate = AttachmentGate::default();
    let first = gate.attach(1).unwrap();
    gate.detach(first).unwrap();
    let second = gate.attach(1).unwrap();

    assert!(gate.detach(first).is_err());
    assert!(gate.is_current(second, 1));
}

#[test]
fn adapter_source_posts_only_weak_revision_gated_ui_closures() {
    let source = fs::read_to_string(spike_root().join("src/desktop.rs")).unwrap();
    for required in [
        "ui_event_channel()",
        "thread::Builder::new()",
        "slint::invoke_from_event_loop",
        "gate.is_current(attachment, revision)",
        "weak.upgrade()",
        "slint::ModelRc::new",
        "pub fn attach(",
        "pub fn detach(",
    ] {
        assert!(
            source.contains(required),
            "missing adapter contract: {required}"
        );
    }
    assert!(!source.contains("tauri"));
    assert!(!source.contains("webview"));
    assert!(!source.contains("posted_consumer_alive.store(false"));
}
