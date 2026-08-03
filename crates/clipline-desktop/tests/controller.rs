use clipline_desktop::{
    ApplyEventOutcome, CloudAccountOwner, CloudAccountScope, CloudUploadProgress,
    CloudUploadUpdateKind, ControllerError, DesktopController, GameDetection, Generation,
    MicMonitor, MicrophonePhase, NoticeKind, RecorderEvent, Revision, UiAction, UiEffect, UiEvent,
    WindowLifecycleMode, WindowLifecycleSnapshot, MAX_ACTIVE_UPLOADS, MAX_NOTICE_MESSAGE_BYTES,
    MAX_PENDING_NOTICES,
};

fn status(generation: u64, recording: bool, segments: usize) -> UiEvent {
    UiEvent::Recorder {
        generation: Generation::new(generation),
        event: RecorderEvent::Status {
            recording,
            waiting_for_game: false,
            segments,
            buffered_s: segments as f64,
            buffered_mb: segments as f64 / 2.0,
            full_session: false,
            encoder: "H.264".to_owned(),
            capture_backend: "windows_graphics_capture".to_owned(),
        },
    }
}

fn game(generation: u64, name: &str) -> UiEvent {
    UiEvent::GameDetection {
        generation: Generation::new(generation),
        detection: GameDetection {
            active: true,
            name: Some(name.to_owned()),
            window_title: Some(name.to_owned()),
            process_id: Some(42),
            process_instance_id: Some(format!("42:{generation}")),
            exe_name: Some("game.exe".to_owned()),
            recording_mode: Some("game".to_owned()),
            elevated_hotkeys_blocked: false,
        },
    }
}

fn cloud_with_status(generation: u64, id: &str, received: u64, status: &str) -> UiEvent {
    cloud_state(
        owner("account-a", 1),
        generation,
        id,
        received,
        status,
        None,
    )
}

fn owner(key: &str, generation: u64) -> CloudAccountOwner {
    CloudAccountOwner::new(key, CloudAccountScope::new(generation)).unwrap()
}

fn account_changed(account: CloudAccountOwner) -> UiEvent {
    UiEvent::CloudAccountChanged {
        account: Some(account),
    }
}

fn cloud_bytes(account: CloudAccountOwner, generation: u64, id: &str, received: u64) -> UiEvent {
    UiEvent::CloudUploadProgress {
        account,
        generation: Generation::new(generation),
        update: CloudUploadUpdateKind::Bytes,
        progress: CloudUploadProgress {
            local_clip_id: id.to_owned(),
            path: format!(r"C:\{id}.mp4"),
            upload_status: "uploading".to_owned(),
            received_size_bytes: received,
            file_size_bytes: 100,
            remote_clip_id: None,
            remote_url: None,
            error: None,
        },
        notice: None,
    }
}

fn cloud_state(
    account: CloudAccountOwner,
    generation: u64,
    id: &str,
    received: u64,
    status: &str,
    notice: Option<&str>,
) -> UiEvent {
    UiEvent::CloudUploadProgress {
        account,
        generation: Generation::new(generation),
        update: CloudUploadUpdateKind::State,
        progress: CloudUploadProgress {
            local_clip_id: id.to_owned(),
            path: format!(r"C:\{id}.mp4"),
            upload_status: status.to_owned(),
            received_size_bytes: received,
            file_size_bytes: 100,
            remote_clip_id: None,
            remote_url: None,
            error: None,
        },
        notice: notice.map(str::to_owned),
    }
}

#[test]
fn account_change_prunes_old_progress_and_rejects_delayed_completion() {
    let mut controller = DesktopController::new((), Vec::new()).unwrap();
    let first = owner("account-a", 1);
    let second = owner("account-b", 2);
    controller
        .apply_event(account_changed(first.clone()))
        .unwrap();
    controller
        .apply_event(cloud_state(
            first.clone(),
            9,
            "same-clip",
            10,
            "uploading",
            None,
        ))
        .unwrap();
    controller
        .apply_event(account_changed(second.clone()))
        .unwrap();
    assert_eq!(controller.snapshot().current_cloud_account, Some(second));
    assert!(controller.snapshot().uploads.is_empty());

    let before = controller.snapshot();
    assert_eq!(
        controller
            .apply_event(cloud_state(
                first,
                8,
                "same-clip",
                99,
                "failed",
                Some("failed"),
            ))
            .unwrap(),
        ApplyEventOutcome::Stale
    );
    assert_eq!(controller.snapshot(), before);
}

#[test]
fn fresh_snapshot_is_complete_and_keeps_exact_settings() {
    let settings = vec!["persisted", "settings"];
    let controller =
        DesktopController::new(settings.clone(), vec!["capture fallback active".to_owned()])
            .unwrap();
    let snapshot = controller.snapshot();
    assert_eq!(snapshot.schema_version, 2);
    assert_eq!(snapshot.revision, Revision::INITIAL);
    assert_eq!(snapshot.settings, settings);
    assert_eq!(snapshot.settings_revision, Revision::INITIAL);
    assert_eq!(snapshot.lifecycle.mode, WindowLifecycleMode::Tray);
    assert!(!snapshot.recorder.status.recording);
    assert_eq!(snapshot.storage, None);
    assert_eq!(snapshot.game.detection, None);
    assert_eq!(snapshot.microphone.phase, MicrophonePhase::Stopped);
    assert!(snapshot.uploads.is_empty());
    assert_eq!(snapshot.current_cloud_account, None);
    assert_eq!(snapshot.library_revision, Revision::INITIAL);
    assert_eq!(snapshot.notices.len(), 1);
    assert_eq!(snapshot.notices[0].kind, NoticeKind::StartupWarning);
}

#[test]
fn coalesced_state_advances_once_and_identical_updates_are_noops() {
    let mut controller = DesktopController::new((), Vec::new()).unwrap();
    assert!(matches!(
        controller.apply_event(status(2, true, 3)).unwrap(),
        ApplyEventOutcome::Applied { revision } if revision == Revision::new(1)
    ));
    let snapshot = controller.snapshot();
    assert!(snapshot.recorder.status.recording);
    assert_eq!(snapshot.recorder.status.segments, 3);
    assert_eq!(snapshot.recorder.generation, Generation::new(2));

    assert_eq!(
        controller.apply_event(status(2, true, 3)).unwrap(),
        ApplyEventOutcome::Unchanged
    );
    assert_eq!(controller.snapshot().revision, Revision::new(1));

    let lifecycle = UiEvent::WindowLifecycle {
        snapshot: WindowLifecycleSnapshot::new(Revision::new(3), WindowLifecycleMode::Foreground),
    };
    controller.apply_event(lifecycle.clone()).unwrap();
    assert_eq!(
        controller.apply_event(lifecycle).unwrap(),
        ApplyEventOutcome::Unchanged
    );
    assert_eq!(controller.snapshot().revision, Revision::new(2));
}

#[test]
fn every_stale_completion_domain_is_rejected_without_mutation() {
    let mut controller = DesktopController::new((), Vec::new()).unwrap();
    controller
        .apply_event(account_changed(owner("account-a", 1)))
        .unwrap();
    controller.apply_event(status(5, true, 1)).unwrap();
    controller.apply_event(game(7, "current")).unwrap();
    controller
        .apply_event(cloud_with_status(9, "clip", 20, "uploading"))
        .unwrap();
    controller
        .apply_event(UiEvent::MicMonitor {
            generation: Generation::new(11),
            monitor: MicMonitor::new(0.1, 0.2, vec![1]).unwrap(),
        })
        .unwrap();
    controller
        .apply_event(UiEvent::EnrichmentUpdated {
            generation: Generation::new(13),
        })
        .unwrap();
    controller
        .apply_event(UiEvent::WindowLifecycle {
            snapshot: WindowLifecycleSnapshot::new(
                Revision::new(15),
                WindowLifecycleMode::Foreground,
            ),
        })
        .unwrap();
    let before = controller.snapshot();

    let stale = [
        status(4, false, 0),
        game(6, "stale"),
        cloud_state(owner("account-a", 1), 8, "clip", 90, "failed", None),
        UiEvent::MicTestStopped {
            generation: Generation::new(10),
        },
        UiEvent::EnrichmentUpdated {
            generation: Generation::new(12),
        },
        UiEvent::WindowLifecycle {
            snapshot: WindowLifecycleSnapshot::new(Revision::new(14), WindowLifecycleMode::Tray),
        },
    ];
    for event in stale {
        assert_eq!(
            controller.apply_event(event).unwrap(),
            ApplyEventOutcome::Stale
        );
        assert_eq!(controller.snapshot(), before);
    }
}

#[test]
fn saved_and_enrichment_events_make_library_rebuild_state_durable() {
    let mut controller = DesktopController::new((), Vec::new()).unwrap();
    controller
        .apply_event(UiEvent::Recorder {
            generation: Generation::new(2),
            event: RecorderEvent::Saved {
                path: r"C:\clip.mp4".to_owned(),
                seconds: 30.0,
                recording_start_unix: Some(10),
                recording_end_unix: Some(40),
                markers: 2,
                full_session: false,
                gc_deleted: 1,
                gc_freed_bytes: 20,
                storage_total_bytes: 100,
                storage_quota_bytes: Some(1_000),
                storage_over_quota: false,
            },
        })
        .unwrap();
    assert_eq!(controller.snapshot().library_revision, Revision::new(1));
    assert_eq!(
        controller.snapshot().latest_saved.as_ref().unwrap().path,
        r"C:\clip.mp4"
    );
    assert_eq!(
        controller.snapshot().storage.as_ref().unwrap().total_bytes,
        100
    );

    controller
        .apply_event(UiEvent::EnrichmentUpdated {
            generation: Generation::new(1),
        })
        .unwrap();
    assert_eq!(controller.snapshot().library_revision, Revision::new(2));

    let rebuilt = DesktopController::from_snapshot(controller.snapshot()).unwrap();
    assert_eq!(rebuilt.snapshot(), controller.snapshot());
}

#[test]
fn notices_are_bounded_fail_atomically_and_acknowledge_idempotently() {
    let warnings = (0..MAX_PENDING_NOTICES)
        .map(|index| format!("warning-{index}"))
        .collect();
    let mut controller = DesktopController::new((), warnings).unwrap();
    let before = controller.snapshot();
    assert_eq!(
        controller.apply_event(UiEvent::UserError {
            message: "overflow".to_owned(),
        }),
        Err(ControllerError::NoticesFull {
            capacity: MAX_PENDING_NOTICES
        })
    );
    assert_eq!(controller.snapshot(), before);

    let notice = controller.snapshot().notices[0].clone();
    let outcome = controller
        .dispatch(UiAction::AcknowledgeNotice {
            notice_id: notice.id,
        })
        .unwrap();
    assert_eq!(outcome.effect, UiEffect::None);
    assert_eq!(controller.snapshot().notices.len(), MAX_PENDING_NOTICES - 1);
    let revision = controller.snapshot().revision;
    let duplicate = controller
        .dispatch(UiAction::AcknowledgeNotice {
            notice_id: notice.id,
        })
        .unwrap();
    assert!(!duplicate.changed);
    assert_eq!(controller.snapshot().revision, revision);
}

#[test]
fn notice_message_bound_fails_atomically() {
    let mut controller = DesktopController::new((), Vec::new()).unwrap();
    let before = controller.snapshot();
    assert_eq!(
        controller.apply_event(UiEvent::UserError {
            message: "x".repeat(MAX_NOTICE_MESSAGE_BYTES + 1),
        }),
        Err(ControllerError::NoticeTooLarge {
            actual: MAX_NOTICE_MESSAGE_BYTES + 1,
            maximum: MAX_NOTICE_MESSAGE_BYTES,
        })
    );
    assert_eq!(controller.snapshot(), before);
}

#[test]
fn account_notice_survives_detached_snapshot_and_requires_exact_ack() {
    let mut controller = DesktopController::new((), Vec::new()).unwrap();
    let account = owner("account-a", 1);
    controller
        .apply_event(account_changed(account.clone()))
        .unwrap();
    let completion = cloud_state(
        account.clone(),
        1,
        "clip",
        100,
        "uploaded_private",
        Some("upload complete"),
    );
    controller.apply_event(completion.clone()).unwrap();

    let rebuilt = DesktopController::from_snapshot(controller.snapshot()).unwrap();
    let notice = rebuilt.snapshot().notices[0].clone();
    assert_eq!(notice.account, Some(account));
    assert_eq!(notice.kind, NoticeKind::CloudUpload);
    assert_eq!(notice.message, "upload complete");

    let mut rebuilt = rebuilt;
    let wrong = rebuilt
        .dispatch(UiAction::AcknowledgeNotice {
            notice_id: notice.id + 1,
        })
        .unwrap();
    assert!(!wrong.changed);
    assert_eq!(rebuilt.snapshot().notices.len(), 1);
    assert!(
        rebuilt
            .dispatch(UiAction::AcknowledgeNotice {
                notice_id: notice.id,
            })
            .unwrap()
            .changed
    );
    assert!(rebuilt.snapshot().notices.is_empty());
    assert_eq!(
        rebuilt.apply_event(completion).unwrap(),
        ApplyEventOutcome::Unchanged
    );
    assert!(rebuilt.snapshot().notices.is_empty());
}

#[test]
fn account_change_prunes_unacknowledged_old_account_notice() {
    let mut controller = DesktopController::new((), Vec::new()).unwrap();
    let first = owner("same-key", 1);
    controller
        .apply_event(account_changed(first.clone()))
        .unwrap();
    controller
        .apply_event(cloud_state(
            first,
            1,
            "clip",
            100,
            "failed",
            Some("upload failed"),
        ))
        .unwrap();
    assert_eq!(controller.snapshot().notices.len(), 1);

    controller
        .apply_event(account_changed(owner("same-key", 2)))
        .unwrap();
    assert!(controller.snapshot().notices.is_empty());
    assert!(controller.snapshot().uploads.is_empty());
}

#[test]
fn byte_progress_changes_snapshot_but_only_state_changes_library_revision() {
    let mut controller = DesktopController::new((), Vec::new()).unwrap();
    let account = owner("account-a", 1);
    controller
        .apply_event(account_changed(account.clone()))
        .unwrap();
    let account_revision = controller.snapshot().library_revision;
    controller
        .apply_event(cloud_state(
            account.clone(),
            1,
            "clip",
            0,
            "uploading",
            None,
        ))
        .unwrap();
    let state_revision = controller.snapshot().library_revision;
    assert!(state_revision > account_revision);

    controller
        .apply_event(cloud_bytes(account.clone(), 1, "clip", 50))
        .unwrap();
    assert_eq!(controller.snapshot().library_revision, state_revision);
    assert_eq!(
        controller.snapshot().uploads[0]
            .progress
            .received_size_bytes,
        50
    );

    controller
        .apply_event(cloud_state(
            account,
            1,
            "clip",
            100,
            "uploaded_private",
            None,
        ))
        .unwrap();
    assert!(controller.snapshot().library_revision > state_revision);
}

#[test]
fn byte_progress_cannot_smuggle_a_state_transition() {
    let mut controller = DesktopController::new((), Vec::new()).unwrap();
    let account = owner("account-a", 1);
    controller
        .apply_event(account_changed(account.clone()))
        .unwrap();
    controller
        .apply_event(cloud_state(
            account.clone(),
            1,
            "clip",
            0,
            "uploading",
            None,
        ))
        .unwrap();
    let mut disguised_state = cloud_bytes(account, 1, "clip", 50);
    let UiEvent::CloudUploadProgress { progress, .. } = &mut disguised_state else {
        unreachable!();
    };
    progress.upload_status = "failed".into();
    let before = controller.snapshot();
    assert_eq!(
        controller.apply_event(disguised_state),
        Err(ControllerError::InvalidCloudProgress(
            "byte-only progress changed upload state"
        ))
    );
    assert_eq!(controller.snapshot(), before);
}

#[test]
fn upload_collection_has_a_hard_deterministic_bound() {
    let mut controller = DesktopController::new((), Vec::new()).unwrap();
    controller
        .apply_event(account_changed(owner("account-a", 1)))
        .unwrap();
    for index in 0..MAX_ACTIVE_UPLOADS {
        controller
            .apply_event(cloud_with_status(
                1,
                &format!("clip-{index:02}"),
                index as u64,
                "uploading",
            ))
            .unwrap();
    }
    let before = controller.snapshot();
    assert_eq!(
        controller.apply_event(cloud_with_status(1, "overflow", 0, "uploading")),
        Err(ControllerError::UploadsFull {
            capacity: MAX_ACTIVE_UPLOADS
        })
    );
    assert_eq!(controller.snapshot(), before);
    let snapshot = controller.snapshot();
    let ids = snapshot
        .uploads
        .iter()
        .map(|upload| upload.progress.local_clip_id.as_str())
        .collect::<Vec<_>>();
    assert!(ids.windows(2).all(|pair| pair[0] < pair[1]));
}

#[test]
fn completed_uploads_are_evicted_oldest_first_under_capacity_pressure() {
    let mut controller = DesktopController::new((), Vec::new()).unwrap();
    controller
        .apply_event(account_changed(owner("account-a", 1)))
        .unwrap();
    for index in 0..MAX_ACTIVE_UPLOADS {
        controller
            .apply_event(cloud_with_status(
                index as u64 + 1,
                &format!("complete-{index:02}"),
                100,
                "uploaded_private",
            ))
            .unwrap();
    }

    controller
        .apply_event(cloud_with_status(100, "new-upload", 0, "uploading"))
        .unwrap();
    let snapshot = controller.snapshot();
    assert_eq!(snapshot.uploads.len(), MAX_ACTIVE_UPLOADS);
    assert!(!snapshot
        .uploads
        .iter()
        .any(|upload| upload.progress.local_clip_id == "complete-00"));
    assert!(snapshot
        .uploads
        .iter()
        .any(|upload| upload.progress.local_clip_id == "new-upload"));
}
