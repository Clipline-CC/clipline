use std::collections::HashSet;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use clipline_desktop::{
    ui_event_channel, CatalogSummarySnapshot, CatalogSummarySource, CloudAccountOwner,
    CloudAccountScope, CloudUploadProgress, CloudUploadUpdateKind, Generation, MicMonitor,
    RecorderEvent, Revision, UiEvent, UiEventPublishOutcome, UiEventReceiveError, UiEventSendError,
    WindowLifecycleMode, WindowLifecycleSnapshot, UI_EVENT_CAPACITY,
};

fn status(generation: u64, segments: usize) -> UiEvent {
    UiEvent::Recorder {
        generation: Generation::new(generation),
        event: RecorderEvent::Status {
            recording: true,
            waiting_for_game: false,
            segments,
            buffered_s: segments as f64,
            buffered_mb: segments as f64,
            full_session: false,
            encoder: String::new(),
            capture_backend: String::new(),
        },
    }
}

fn catalog(revision: u64, source: CatalogSummarySource, active: bool) -> UiEvent {
    UiEvent::CatalogSummaryChanged {
        summary: CatalogSummarySnapshot {
            revision: Revision::new(revision),
            source,
            active,
        },
    }
}

#[test]
fn catalog_summaries_are_revision_fenced_and_coalesce_only_before_barriers() {
    let (sender, receiver) = ui_event_channel();
    assert_eq!(
        sender.try_publish(catalog(1, CatalogSummarySource::Local, false)),
        Ok(UiEventPublishOutcome::Queued)
    );
    assert_eq!(
        sender.try_publish(catalog(2, CatalogSummarySource::Cloud, true)),
        Ok(UiEventPublishOutcome::Replaced)
    );
    assert!(matches!(
        sender.try_publish(catalog(1, CatalogSummarySource::Local, false)),
        Err(UiEventSendError::StaleRevision { .. })
    ));
    assert!(matches!(
        sender.try_publish(catalog(2, CatalogSummarySource::Local, false)),
        Err(UiEventSendError::StaleRevision { .. })
    ));
    sender
        .try_publish(UiEvent::UserError {
            message: "barrier".into(),
        })
        .unwrap();
    assert_eq!(
        sender.try_publish(catalog(3, CatalogSummarySource::Local, false)),
        Ok(UiEventPublishOutcome::Queued)
    );
    assert!(matches!(
        receiver.try_recv().unwrap().event,
        UiEvent::CatalogSummaryChanged { .. }
    ));
    assert!(matches!(
        receiver.try_recv().unwrap().event,
        UiEvent::UserError { .. }
    ));
    assert!(matches!(
        receiver.try_recv().unwrap().event,
        UiEvent::CatalogSummaryChanged { .. }
    ));
}

fn cloud(generation: u64, id: impl Into<String>, received: u64) -> UiEvent {
    cloud_bytes(owner("account-a", 1), generation, id, received)
}

fn owner(key: &str, generation: u64) -> CloudAccountOwner {
    CloudAccountOwner::new(key, CloudAccountScope::new(generation)).unwrap()
}

fn account_changed(account: CloudAccountOwner) -> UiEvent {
    UiEvent::CloudAccountChanged {
        generation: account.account_generation(),
        account: Some(account),
    }
}

fn disconnected(generation: u64) -> UiEvent {
    UiEvent::CloudAccountChanged {
        generation: CloudAccountScope::new(generation),
        account: None,
    }
}

#[test]
fn delayed_connect_or_disconnect_cannot_reorder_account_ownership() {
    let (sender, receiver) = ui_event_channel();
    let current = owner("account-b", 4);
    sender
        .try_publish(account_changed(current.clone()))
        .unwrap();
    assert!(matches!(
        sender.try_publish(disconnected(3)),
        Err(UiEventSendError::StaleAccount { .. })
    ));
    assert!(matches!(
        sender.try_publish(account_changed(owner("account-a", 2))),
        Err(UiEventSendError::StaleAccount { .. })
    ));
    sender.try_publish(disconnected(5)).unwrap();
    assert_eq!(receiver.len(), 2);
}

fn cloud_bytes(
    account: CloudAccountOwner,
    account_generation: u64,
    id: impl Into<String>,
    received: u64,
) -> UiEvent {
    let local_clip_id = id.into();
    UiEvent::CloudUploadProgress {
        account,
        generation: Generation::new(account_generation),
        update: CloudUploadUpdateKind::Bytes,
        progress: CloudUploadProgress {
            path: format!(r"C:\{local_clip_id}.mp4"),
            local_clip_id,
            upload_status: "uploading".to_owned(),
            terminal: false,
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
    id: impl Into<String>,
    status: &str,
    notice: Option<&str>,
) -> UiEvent {
    let local_clip_id = id.into();
    UiEvent::CloudUploadProgress {
        account,
        generation: Generation::new(generation),
        update: CloudUploadUpdateKind::State,
        progress: CloudUploadProgress {
            path: format!(r"C:\{local_clip_id}.mp4"),
            local_clip_id,
            upload_status: status.to_owned(),
            terminal: false,
            received_size_bytes: 0,
            file_size_bytes: 100,
            remote_clip_id: None,
            remote_url: None,
            error: None,
        },
        notice: notice.map(str::to_owned),
    }
}

fn cloud_removed(account: CloudAccountOwner, generation: u64, id: &str) -> UiEvent {
    UiEvent::CloudUploadRemoved {
        account,
        generation: Generation::new(generation),
        local_clip_id: id.to_owned(),
    }
}

#[test]
fn cloud_progress_never_coalesces_or_stales_across_accounts() {
    let (sender, receiver) = ui_event_channel();
    let first = owner("same-key", 1);
    let second = owner("same-key", 2);

    sender.try_publish(account_changed(first.clone())).unwrap();
    assert_eq!(
        sender
            .try_publish(cloud_state(
                first.clone(),
                9,
                "same-clip",
                "uploading",
                None
            ))
            .unwrap(),
        UiEventPublishOutcome::Queued
    );
    assert_eq!(
        sender
            .try_publish(cloud_bytes(first.clone(), 9, "same-clip", 20))
            .unwrap(),
        UiEventPublishOutcome::Queued
    );
    assert_eq!(
        sender
            .try_publish(cloud_bytes(first.clone(), 9, "same-clip", 30))
            .unwrap(),
        UiEventPublishOutcome::Replaced
    );

    sender.try_publish(account_changed(second.clone())).unwrap();
    assert_eq!(
        sender.try_publish(cloud_bytes(first, 9, "same-clip", 40)),
        Err(UiEventSendError::AccountChanged)
    );
    assert_eq!(
        sender
            .try_publish(cloud_state(
                second.clone(),
                1,
                "same-clip",
                "uploading",
                None
            ))
            .unwrap(),
        UiEventPublishOutcome::Queued
    );
    assert_eq!(
        sender
            .try_publish(cloud_bytes(second, 1, "same-clip", 50))
            .unwrap(),
        UiEventPublishOutcome::Queued
    );
    assert_eq!(receiver.len(), 6);
}

#[test]
fn cloud_state_and_account_changes_are_durable_barriers() {
    let (sender, receiver) = ui_event_channel();
    let account = owner("account-a", 1);
    sender
        .try_publish(account_changed(account.clone()))
        .unwrap();
    sender
        .try_publish(cloud_state(account.clone(), 1, "clip", "uploading", None))
        .unwrap();
    sender
        .try_publish(cloud_bytes(account.clone(), 1, "clip", 10))
        .unwrap();
    sender
        .try_publish(cloud_state(
            account.clone(),
            1,
            "clip",
            "uploaded_private",
            Some("uploaded"),
        ))
        .unwrap();
    sender
        .try_publish(cloud_bytes(account, 1, "clip", 100))
        .unwrap();

    assert_eq!(receiver.len(), 5);
}

#[test]
fn cloud_upload_removal_is_an_account_fenced_durable_barrier() {
    let (sender, receiver) = ui_event_channel();
    let account = owner("account-a", 1);
    sender
        .try_publish(account_changed(account.clone()))
        .unwrap();
    sender
        .try_publish(cloud_bytes(account.clone(), 7, "clip", 10))
        .unwrap();
    sender
        .try_publish(cloud_removed(account.clone(), 7, "clip"))
        .unwrap();
    sender
        .try_publish(cloud_bytes(account.clone(), 7, "clip", 20))
        .unwrap();
    assert_eq!(receiver.len(), 4, "removal separates byte progress epochs");

    assert_eq!(
        sender.try_publish(cloud_removed(owner("account-b", 2), 7, "clip")),
        Err(UiEventSendError::AccountChanged)
    );
}

#[test]
fn explicit_terminal_upload_state_remains_a_durable_barrier() {
    let (sender, receiver) = ui_event_channel();
    let account = owner("account-a", 1);
    sender
        .try_publish(account_changed(account.clone()))
        .unwrap();
    let mut terminal = cloud_state(
        account.clone(),
        1,
        "clip",
        "uploaded_processing",
        Some("preserved"),
    );
    let UiEvent::CloudUploadProgress { progress, .. } = &mut terminal else {
        unreachable!();
    };
    progress.terminal = true;
    sender.try_publish(terminal).unwrap();
    sender
        .try_publish(cloud_bytes(account, 1, "clip", 100))
        .unwrap();

    assert_eq!(receiver.len(), 3);
    let terminal = receiver.try_recv().unwrap();
    assert!(matches!(
        terminal.event,
        UiEvent::CloudAccountChanged { .. }
    ));
    let terminal = receiver.try_recv().unwrap();
    let UiEvent::CloudUploadProgress { progress, .. } = terminal.event else {
        panic!("expected terminal upload event");
    };
    assert!(progress.terminal);
}

#[test]
fn invalid_terminal_upload_state_is_rejected_before_it_reaches_the_queue() {
    let (sender, receiver) = ui_event_channel();
    let account = owner("account-a", 1);
    sender
        .try_publish(account_changed(account.clone()))
        .unwrap();
    receiver.try_recv().unwrap();
    let mut invalid = cloud_state(account, 1, "clip", "uploading", None);
    let UiEvent::CloudUploadProgress { progress, .. } = &mut invalid else {
        unreachable!();
    };
    progress.terminal = true;

    assert_eq!(
        sender.try_publish(invalid),
        Err(UiEventSendError::InvalidCloudProgress(
            "terminal signal is inconsistent with upload status"
        ))
    );
    assert!(receiver.is_empty());
}

#[test]
fn coalescing_is_last_writer_wins_without_crossing_a_durable_barrier() {
    let (sender, receiver) = ui_event_channel();
    assert_eq!(
        sender.try_publish(status(1, 1)).unwrap(),
        UiEventPublishOutcome::Queued
    );
    assert_eq!(
        sender.try_publish(status(1, 2)).unwrap(),
        UiEventPublishOutcome::Replaced
    );
    let latest = receiver.try_recv().unwrap();
    assert_eq!(latest.sequence, 2);
    assert!(matches!(
        latest.event,
        UiEvent::Recorder {
            event: RecorderEvent::Status { segments: 2, .. },
            ..
        }
    ));

    sender.try_publish(status(1, 3)).unwrap();
    sender
        .try_publish(UiEvent::UserError {
            message: "durable".to_owned(),
        })
        .unwrap();
    sender.try_publish(status(1, 4)).unwrap();
    assert_eq!(receiver.len(), 3);
    assert!(matches!(
        receiver.try_recv().unwrap().event,
        UiEvent::Recorder {
            event: RecorderEvent::Status { segments: 3, .. },
            ..
        }
    ));
    assert!(matches!(
        receiver.try_recv().unwrap().event,
        UiEvent::UserError { .. }
    ));
    assert!(matches!(
        receiver.try_recv().unwrap().event,
        UiEvent::Recorder {
            event: RecorderEvent::Status { segments: 4, .. },
            ..
        }
    ));
}

#[test]
fn coalescing_preserves_monotonic_delivery_order() {
    let (sender, receiver) = ui_event_channel();
    sender.try_publish(status(1, 1)).unwrap();
    sender
        .try_publish(UiEvent::GameDetection {
            generation: Generation::new(1),
            detection: clipline_desktop::GameDetection {
                active: false,
                name: None,
                window_title: None,
                process_id: None,
                process_instance_id: None,
                exe_name: None,
                recording_mode: None,
                elevated_hotkeys_blocked: false,
            },
        })
        .unwrap();
    sender.try_publish(status(1, 2)).unwrap();

    let first = receiver.try_recv().unwrap();
    let second = receiver.try_recv().unwrap();
    assert_eq!((first.sequence, second.sequence), (2, 3));
    assert!(matches!(first.event, UiEvent::GameDetection { .. }));
    assert!(matches!(
        second.event,
        UiEvent::Recorder {
            event: RecorderEvent::Status { segments: 2, .. },
            ..
        }
    ));
}

#[test]
fn capacity_reserves_one_terminal_slot_and_full_is_atomic() {
    let (sender, receiver) = ui_event_channel();
    let account = owner("account-a", 1);
    sender
        .try_publish(account_changed(account.clone()))
        .unwrap();
    receiver.try_recv().unwrap();
    for index in 0..(UI_EVENT_CAPACITY - 1) {
        sender
            .try_publish(cloud_state(
                account.clone(),
                1,
                format!("clip-{index}"),
                "uploading",
                None,
            ))
            .unwrap();
        receiver.try_recv().unwrap();
        sender
            .try_publish(cloud(1, format!("clip-{index}"), index as u64))
            .unwrap();
    }
    assert_eq!(receiver.len(), UI_EVENT_CAPACITY - 1);
    assert_eq!(
        sender.try_publish(cloud(1, "overflow", 1)),
        Err(UiEventSendError::Full {
            capacity: UI_EVENT_CAPACITY
        })
    );
    assert_eq!(receiver.len(), UI_EVENT_CAPACITY - 1);

    sender
        .try_publish(UiEvent::MicTestStopped {
            generation: Generation::new(3),
        })
        .unwrap();
    assert_eq!(receiver.len(), UI_EVENT_CAPACITY);
    assert_eq!(
        sender.try_publish(UiEvent::UserError {
            message: "still full".to_owned(),
        }),
        Err(UiEventSendError::Full {
            capacity: UI_EVENT_CAPACITY
        })
    );
    assert_eq!(receiver.len(), UI_EVENT_CAPACITY);
}

#[test]
fn stale_generations_and_post_terminal_microphone_samples_are_rejected() {
    let (sender, receiver) = ui_event_channel();
    sender.try_publish(status(4, 1)).unwrap();
    assert_eq!(
        sender.try_publish(status(3, 2)),
        Err(UiEventSendError::Stale {
            current: Generation::new(4),
            received: Generation::new(3),
        })
    );
    sender
        .try_publish(UiEvent::MicTestStopped {
            generation: Generation::new(8),
        })
        .unwrap();
    assert_eq!(
        sender.try_publish(UiEvent::MicMonitor {
            generation: Generation::new(8),
            monitor: MicMonitor::new(0.0, 0.0, Vec::new()).unwrap(),
        }),
        Err(UiEventSendError::Stale {
            current: Generation::new(8),
            received: Generation::new(8),
        })
    );

    sender
        .try_publish(UiEvent::WindowLifecycle {
            snapshot: WindowLifecycleSnapshot::new(
                Revision::new(5),
                WindowLifecycleMode::Foreground,
            ),
        })
        .unwrap();
    assert_eq!(
        sender.try_publish(UiEvent::WindowLifecycle {
            snapshot: WindowLifecycleSnapshot::new(Revision::new(4), WindowLifecycleMode::Tray,),
        }),
        Err(UiEventSendError::StaleRevision {
            current: Revision::new(5),
            received: Revision::new(4),
        })
    );
    assert_eq!(receiver.len(), 3);
}

#[test]
fn disconnection_and_bounded_wait_are_explicit() {
    let (sender, receiver) = ui_event_channel();
    let started = Instant::now();
    assert_eq!(receiver.wait_recv(Duration::from_millis(10)), Ok(None));
    assert!(started.elapsed() < Duration::from_secs(1));
    drop(receiver);
    assert_eq!(
        sender.try_publish(status(1, 1)),
        Err(UiEventSendError::Disconnected)
    );

    let (sender, receiver) = ui_event_channel();
    drop(sender);
    assert_eq!(
        receiver.wait_recv(Duration::from_secs(1)),
        Err(UiEventReceiveError::Disconnected)
    );
}

#[test]
fn concurrent_publishers_never_exceed_capacity_or_duplicate_sequences() {
    let (sender, receiver) = ui_event_channel();
    let account = owner("account-a", 1);
    sender
        .try_publish(account_changed(account.clone()))
        .unwrap();
    receiver.try_recv().unwrap();
    let barrier = Arc::new(Barrier::new(5));
    let mut threads = Vec::new();
    for worker in 0..4 {
        let sender = sender.clone();
        let barrier = Arc::clone(&barrier);
        let account = account.clone();
        threads.push(thread::spawn(move || {
            barrier.wait();
            for index in 0..UI_EVENT_CAPACITY {
                let id = format!("worker-{worker}-clip-{index}");
                let result =
                    sender.try_publish(cloud_state(account.clone(), 1, id, "uploading", None));
                assert!(result.is_ok() || matches!(result, Err(UiEventSendError::Full { .. })));
            }
        }));
    }
    barrier.wait();
    for thread in threads {
        thread.join().unwrap();
    }
    assert!(receiver.len() <= UI_EVENT_CAPACITY);
    let mut sequences = HashSet::new();
    while let Some(update) = receiver.try_recv() {
        assert!(sequences.insert(update.sequence));
    }
}
