use clipline_desktop::{
    CatalogSummarySnapshot, CatalogSummarySource, CloudAccountOwner, CloudAccountScope,
    CloudUploadProgress, CloudUploadUpdateKind, DesktopController, GameDetection, Generation,
    GenerationError, MicMonitor, RecorderEvent, Revision, UiAction, UiEffect, UiEvent,
    WindowLifecycleMode, WindowLifecycleSnapshot, MAX_CLOUD_ACCOUNT_KEY_BYTES,
    MAX_MIC_MONITOR_SAMPLES,
};
use serde_json::json;

#[test]
fn actions_are_owned_typed_and_map_to_shell_independent_effects() {
    let save = UiAction::SaveReplay;
    assert_eq!(save.effect(), UiEffect::RequestSaveReplay);
    assert_eq!(
        UiAction::SetRecording { recording: true }.effect(),
        UiEffect::SetRecording { recording: true }
    );
    assert_eq!(
        UiAction::SetLifecycle {
            mode: WindowLifecycleMode::Taskbar,
        }
        .effect(),
        UiEffect::SetLifecycle {
            mode: WindowLifecycleMode::Taskbar,
        }
    );
    assert_eq!(
        serde_json::to_value(save).unwrap(),
        json!({ "kind": "save_replay" })
    );
}

#[test]
fn generations_and_revisions_never_wrap() {
    assert_eq!(Generation::INITIAL.checked_next().unwrap().get(), 1);
    assert_eq!(Revision::INITIAL.checked_next().unwrap().get(), 1);
    assert_eq!(
        Generation::new(u64::MAX).checked_next(),
        Err(GenerationError::Exhausted)
    );
    assert_eq!(
        Revision::new(u64::MAX).checked_next(),
        Err(GenerationError::Exhausted)
    );
}

#[test]
fn recorder_payloads_keep_the_shipping_json_contract() {
    let status = RecorderEvent::Status {
        recording: true,
        waiting_for_game: false,
        segments: 3,
        buffered_s: 12.5,
        buffered_mb: 8.25,
        full_session: true,
        encoder: "AMD AMF · H.264".to_owned(),
        capture_backend: "windows_graphics_capture".to_owned(),
    };
    assert_eq!(
        serde_json::to_value(status).unwrap(),
        json!({
            "kind": "status",
            "recording": true,
            "waiting_for_game": false,
            "segments": 3,
            "buffered_s": 12.5,
            "buffered_mb": 8.25,
            "full_session": true,
            "encoder": "AMD AMF · H.264",
            "capture_backend": "windows_graphics_capture"
        })
    );

    let saved = RecorderEvent::Saved {
        path: r"C:\Videos\Clipline\clip.mp4".to_owned(),
        seconds: 30.0,
        recording_start_unix: Some(10),
        recording_end_unix: Some(40),
        markers: 2,
        full_session: false,
        gc_deleted: 1,
        gc_freed_bytes: 512,
        storage_total_bytes: 1024,
        storage_quota_bytes: Some(2048),
        storage_over_quota: false,
    };
    assert_eq!(
        serde_json::to_value(saved).unwrap(),
        json!({
            "kind": "saved",
            "path": r"C:\Videos\Clipline\clip.mp4",
            "seconds": 30.0,
            "recording_start_unix": 10,
            "recording_end_unix": 40,
            "markers": 2,
            "full_session": false,
            "gc_deleted": 1,
            "gc_freed_bytes": 512,
            "storage_total_bytes": 1024,
            "storage_quota_bytes": 2048,
            "storage_over_quota": false
        })
    );
}

#[test]
fn lifecycle_and_stream_payloads_are_bounded_owned_values() {
    let lifecycle = WindowLifecycleSnapshot::new(Revision::new(7), WindowLifecycleMode::Tray);
    assert!(lifecycle.backgrounded);
    assert_eq!(
        serde_json::to_value(lifecycle).unwrap(),
        json!({ "revision": 7, "mode": "tray", "backgrounded": true })
    );

    let monitor = MicMonitor::new(0.25, 0.5, vec![1, -2, 3]).unwrap();
    assert_eq!(monitor.sample_count, 3);
    assert_eq!(monitor.samples, vec![1, -2, 3]);
    assert!(MicMonitor::new(0.0, 0.0, vec![0; MAX_MIC_MONITOR_SAMPLES + 1]).is_err());
    assert!(MicMonitor::new(f32::NAN, 0.0, Vec::new()).is_err());
}

#[test]
fn ui_events_carry_generations_for_stale_completion_domains() {
    let game = GameDetection {
        active: true,
        name: Some("League of Legends".to_owned()),
        window_title: Some("League of Legends".to_owned()),
        process_id: Some(42),
        process_instance_id: Some("42:100".to_owned()),
        exe_name: Some("LeagueClient.exe".to_owned()),
        recording_mode: Some("game".to_owned()),
        elevated_hotkeys_blocked: false,
    };
    let event = UiEvent::GameDetection {
        generation: Generation::new(9),
        detection: game,
    };
    assert_eq!(event.generation(), Some(Generation::new(9)));

    let progress = CloudUploadProgress {
        local_clip_id: "local-1".to_owned(),
        path: r"C:\clip.mp4".to_owned(),
        upload_status: "uploading".to_owned(),
        terminal: false,
        received_size_bytes: 5,
        file_size_bytes: 10,
        remote_clip_id: None,
        remote_url: None,
        error: None,
    };
    let event = UiEvent::CloudUploadProgress {
        account: CloudAccountOwner::new("account-a", CloudAccountScope::new(7)).unwrap(),
        generation: Generation::new(4),
        update: CloudUploadUpdateKind::Bytes,
        progress,
        notice: None,
    };
    assert_eq!(event.generation(), Some(Generation::new(4)));
}

#[test]
fn cloud_terminal_signal_is_backward_compatible_and_only_serialized_when_set() {
    let legacy = json!({
        "local_clip_id": "local-1",
        "path": r"C:\clip.mp4",
        "upload_status": "uploading",
        "received_size_bytes": 5,
        "file_size_bytes": 10,
        "remote_clip_id": null,
        "error": null
    });
    let progress: CloudUploadProgress = serde_json::from_value(legacy.clone()).unwrap();
    assert!(!progress.terminal);
    assert_eq!(serde_json::to_value(&progress).unwrap(), legacy);

    let mut abandoned = progress;
    abandoned.upload_status = "uploaded_processing".into();
    abandoned.terminal = true;
    assert_eq!(
        serde_json::to_value(abandoned).unwrap()["terminal"],
        json!(true)
    );
}

#[test]
fn cloud_account_owners_are_bounded_exact_values() {
    let owner = CloudAccountOwner::new("account-a", CloudAccountScope::new(7)).unwrap();
    assert_eq!(owner.account_key(), "account-a");
    assert_eq!(owner.account_generation(), CloudAccountScope::new(7));
    assert_eq!(
        serde_json::to_value(&owner).unwrap(),
        json!({ "account_key": "account-a", "account_generation": 7 })
    );
    assert!(CloudAccountOwner::new("", CloudAccountScope::new(1)).is_err());
    assert!(CloudAccountOwner::new(
        "x".repeat(MAX_CLOUD_ACCOUNT_KEY_BYTES + 1),
        CloudAccountScope::new(1)
    )
    .is_err());
    assert!(serde_json::from_value::<CloudAccountOwner>(json!({
        "account_key": "",
        "account_generation": 1
    }))
    .is_err());
}

#[test]
fn desktop_catalog_summary_is_fixed_shape_and_contains_no_item_arrays() {
    let summary = CatalogSummarySnapshot {
        revision: Revision::new(9),
        source: CatalogSummarySource::Cloud,
        active: true,
    };
    assert_eq!(
        serde_json::to_value(summary).unwrap(),
        json!({ "revision": 9, "source": "cloud", "active": true })
    );

    let snapshot =
        serde_json::to_value(DesktopController::new((), Vec::new()).unwrap().snapshot()).unwrap();
    assert_eq!(snapshot["schema_version"], json!(5));
    assert_eq!(
        snapshot["catalog"],
        json!({ "revision": 0, "source": "local", "active": false })
    );
    let catalog = snapshot["catalog"].as_object().unwrap();
    assert_eq!(catalog.len(), 3);
    for forbidden in ["items", "rows", "groups", "selected", "uploads"] {
        assert!(!catalog.contains_key(forbidden));
    }
    assert_eq!(snapshot["settings_probes"], json!([]));
}
