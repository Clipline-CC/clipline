use std::collections::HashSet;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use clipline_desktop::{
    ui_event_channel, CloudAccountScope, CloudUploadProgress, Generation, MicMonitor,
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

fn cloud(generation: u64, id: impl Into<String>, received: u64) -> UiEvent {
    cloud_for_account(1, generation, id, received)
}

fn cloud_for_account(
    account_generation: u64,
    generation: u64,
    id: impl Into<String>,
    received: u64,
) -> UiEvent {
    let local_clip_id = id.into();
    UiEvent::CloudUploadProgress {
        account: CloudAccountScope::new(account_generation),
        generation: Generation::new(generation),
        progress: CloudUploadProgress {
            path: format!(r"C:\{local_clip_id}.mp4"),
            local_clip_id,
            upload_status: "uploading".to_owned(),
            received_size_bytes: received,
            file_size_bytes: 100,
            remote_clip_id: None,
            remote_url: None,
            error: None,
        },
    }
}

#[test]
fn cloud_progress_never_coalesces_or_stales_across_accounts() {
    let (sender, receiver) = ui_event_channel();

    assert_eq!(
        sender
            .try_publish(cloud_for_account(1, 9, "same-clip", 10))
            .unwrap(),
        UiEventPublishOutcome::Queued
    );
    assert_eq!(
        sender
            .try_publish(cloud_for_account(2, 1, "same-clip", 20))
            .unwrap(),
        UiEventPublishOutcome::Queued
    );
    assert_eq!(receiver.len(), 2);

    assert_eq!(
        sender.try_publish(cloud_for_account(1, 8, "same-clip", 30)),
        Err(UiEventSendError::Stale {
            current: Generation::new(9),
            received: Generation::new(8),
        })
    );
    assert_eq!(
        sender
            .try_publish(cloud_for_account(2, 2, "same-clip", 40))
            .unwrap(),
        UiEventPublishOutcome::Replaced
    );
    assert_eq!(receiver.len(), 2);
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
    for index in 0..(UI_EVENT_CAPACITY - 1) {
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
    let barrier = Arc::new(Barrier::new(5));
    let mut threads = Vec::new();
    for worker in 0..4 {
        let sender = sender.clone();
        let barrier = Arc::clone(&barrier);
        threads.push(thread::spawn(move || {
            barrier.wait();
            for index in 0..UI_EVENT_CAPACITY {
                let result = sender.try_publish(cloud(
                    1,
                    format!("worker-{worker}-clip-{index}"),
                    index as u64,
                ));
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
