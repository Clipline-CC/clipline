use std::path::PathBuf;

use clipline_mp4::{IndexedMovie, PlaybackTrackConfig};
use clipline_playback::{
    BackendComponent, BackendError, BackendErrorKind, EnqueueOutcome, MonotonicTime100ns,
    PlaybackCommand, PlaybackEvent, PlaybackPhase, PlaybackTime, PlaybackWorker,
    RecoveryDisposition, WorkerAction, WorkerActionKind, WorkerCompletion, WorkerSeekPlan,
    MAX_PIPELINE_RECOVERY_ATTEMPTS,
};

fn at(ticks: u64, timescale: u32) -> PlaybackTime {
    PlaybackTime::new(ticks, timescale).unwrap()
}

fn next(worker: &mut PlaybackWorker) -> WorkerAction {
    worker.next_action().unwrap().expect("worker action")
}

fn complete(worker: &mut PlaybackWorker, action: &WorkerAction, output: WorkerCompletion) {
    assert!(worker.complete(action, output).unwrap());
}

fn plan(target: PlaybackTime, sample_index: usize) -> WorkerSeekPlan {
    WorkerSeekPlan::new(target, sample_index, sample_index).unwrap()
}

fn drive_open(worker: &mut PlaybackWorker, path: &str) {
    worker
        .enqueue(
            PlaybackCommand::Open {
                path: PathBuf::from(path),
            },
            MonotonicTime100ns::new(10),
        )
        .unwrap();
    let indexed = next(worker);
    assert!(matches!(indexed.kind(), WorkerActionKind::IndexOpen { .. }));
    complete(
        worker,
        &indexed,
        WorkerCompletion::Indexed {
            duration: at(10, 1),
            video_sample_count: 10,
        },
    );
    let flush = next(worker);
    assert!(matches!(flush.kind(), WorkerActionKind::Flush));
    complete(worker, &flush, WorkerCompletion::Done);
    let seek = next(worker);
    assert!(matches!(
        seek.kind(),
        WorkerActionKind::PlanSeek {
            step_frames: None,
            ..
        }
    ));
    complete(
        worker,
        &seek,
        WorkerCompletion::SeekPlanned(plan(at(0, 1), 0)),
    );
    for expected in [
        WorkerActionKind::ReadVideo { sample_index: 0 },
        WorkerActionKind::ConvertVideo { sample_index: 0 },
        WorkerActionKind::DecodeVideo { sample_index: 0 },
        WorkerActionKind::ProduceAudio,
    ] {
        let action = next(worker);
        assert_eq!(action.kind(), &expected);
        complete(worker, &action, WorkerCompletion::Done);
    }
    let publish = next(worker);
    assert_eq!(
        publish.kind(),
        &WorkerActionKind::PublishVideo { sample_index: 0 }
    );
    complete(
        worker,
        &publish,
        WorkerCompletion::Published { position: at(0, 1) },
    );
}

fn drive_seek(worker: &mut PlaybackWorker, target: PlaybackTime, sample_index: usize) {
    let flush = next(worker);
    assert_eq!(flush.kind(), &WorkerActionKind::Flush);
    complete(worker, &flush, WorkerCompletion::Done);
    let seek = next(worker);
    assert!(matches!(seek.kind(), WorkerActionKind::PlanSeek { .. }));
    complete(
        worker,
        &seek,
        WorkerCompletion::SeekPlanned(plan(target, sample_index)),
    );
    for expected in [
        WorkerActionKind::ReadVideo { sample_index },
        WorkerActionKind::ConvertVideo { sample_index },
        WorkerActionKind::DecodeVideo { sample_index },
        WorkerActionKind::ProduceAudio,
    ] {
        let action = next(worker);
        assert_eq!(action.kind(), &expected);
        complete(worker, &action, WorkerCompletion::Done);
    }
    let publish = next(worker);
    assert_eq!(
        publish.kind(),
        &WorkerActionKind::PublishVideo { sample_index }
    );
    complete(
        worker,
        &publish,
        WorkerCompletion::Published { position: target },
    );
}

#[test]
fn open_publishes_the_initial_frame_before_opened_and_stays_paused() {
    let mut worker = PlaybackWorker::new();
    drive_open(&mut worker, "clip.mp4");

    assert_eq!(worker.snapshot().phase, PlaybackPhase::Paused);
    assert_eq!(worker.snapshot().position, at(0, 1));
    assert!(worker.next_action().unwrap().is_none());
    let events = worker.take_events();
    assert!(matches!(events.as_slice(), [PlaybackEvent::Opened { .. }]));
}

#[test]
fn rapid_seek_cancels_each_pipeline_checkpoint_and_only_final_seek_settles() {
    for checkpoint in 0..7 {
        let mut worker = PlaybackWorker::new();
        drive_open(&mut worker, "clip.mp4");
        worker.take_events();
        worker
            .enqueue(
                PlaybackCommand::Seek { position: at(2, 1) },
                MonotonicTime100ns::new(100),
            )
            .unwrap();

        let mut stale_action = next(&mut worker);
        for _ in 0..checkpoint {
            let completion = match stale_action.kind() {
                WorkerActionKind::PlanSeek { .. } => {
                    WorkerCompletion::SeekPlanned(plan(at(2, 1), 2))
                }
                WorkerActionKind::PublishVideo { .. } => {
                    WorkerCompletion::Published { position: at(2, 1) }
                }
                _ => WorkerCompletion::Done,
            };
            complete(&mut worker, &stale_action, completion);
            stale_action = next(&mut worker);
        }

        worker
            .enqueue(
                PlaybackCommand::Seek { position: at(7, 1) },
                MonotonicTime100ns::new(250),
            )
            .unwrap();
        let final_flush = next(&mut worker);
        assert_eq!(final_flush.kind(), &WorkerActionKind::Flush);
        assert_ne!(final_flush.token(), stale_action.token());
        assert!(!worker
            .complete(&stale_action, WorkerCompletion::Done)
            .unwrap());
        complete(&mut worker, &final_flush, WorkerCompletion::Done);
        let plan_action = next(&mut worker);
        complete(
            &mut worker,
            &plan_action,
            WorkerCompletion::SeekPlanned(plan(at(7, 1), 7)),
        );
        for _ in 0..4 {
            let action = next(&mut worker);
            complete(&mut worker, &action, WorkerCompletion::Done);
        }
        let publish = next(&mut worker);
        complete(
            &mut worker,
            &publish,
            WorkerCompletion::Published { position: at(7, 1) },
        );

        assert_eq!(worker.snapshot().position, at(7, 1));
        assert!(matches!(
            worker.take_events().as_slice(),
            [PlaybackEvent::SeekSettled { position, .. }] if *position == at(7, 1)
        ));
        assert_eq!(worker.stale_completions(), 1);
    }
}

#[test]
fn close_during_open_or_seek_releases_the_pipeline_and_fences_old_completion() {
    let mut opening = PlaybackWorker::new();
    opening
        .enqueue(
            PlaybackCommand::Open {
                path: PathBuf::from("clip.mp4"),
            },
            MonotonicTime100ns::new(0),
        )
        .unwrap();
    let stale_open = next(&mut opening);
    opening
        .enqueue(PlaybackCommand::Close, MonotonicTime100ns::new(1))
        .unwrap();
    let close = next(&mut opening);
    assert_eq!(close.kind(), &WorkerActionKind::CloseBackends);
    assert!(!opening
        .complete(
            &stale_open,
            WorkerCompletion::Indexed {
                duration: at(10, 1),
                video_sample_count: 10,
            }
        )
        .unwrap());
    complete(&mut opening, &close, WorkerCompletion::Done);
    assert_eq!(opening.snapshot().phase, PlaybackPhase::Closed);
    assert!(matches!(
        opening.take_events().as_slice(),
        [PlaybackEvent::Closed { .. }]
    ));

    let mut seeking = PlaybackWorker::new();
    drive_open(&mut seeking, "clip.mp4");
    seeking.take_events();
    seeking
        .enqueue(
            PlaybackCommand::Seek { position: at(5, 1) },
            MonotonicTime100ns::new(2),
        )
        .unwrap();
    let stale_seek = next(&mut seeking);
    seeking
        .enqueue(PlaybackCommand::Close, MonotonicTime100ns::new(3))
        .unwrap();
    let close = next(&mut seeking);
    assert!(!seeking
        .complete(&stale_seek, WorkerCompletion::Done)
        .unwrap());
    complete(&mut seeking, &close, WorkerCompletion::Done);
    assert!(matches!(
        seeking.take_events().as_slice(),
        [PlaybackEvent::Closed { .. }]
    ));
}

#[test]
fn step_track_volume_and_transport_intent_reach_the_backend_contract() {
    let mut worker = PlaybackWorker::new();
    drive_open(&mut worker, "clip.mp4");
    worker.take_events();

    worker
        .enqueue(
            PlaybackCommand::SetVolume { volume: 0.25 },
            MonotonicTime100ns::new(1),
        )
        .unwrap();
    let volume = next(&mut worker);
    assert_eq!(volume.kind(), &WorkerActionKind::SetVolume { volume: 0.25 });
    complete(&mut worker, &volume, WorkerCompletion::Done);

    worker
        .enqueue(PlaybackCommand::Play, MonotonicTime100ns::new(2))
        .unwrap();
    let play = next(&mut worker);
    assert_eq!(
        play.kind(),
        &WorkerActionKind::SetTransport { playing: true }
    );
    complete(&mut worker, &play, WorkerCompletion::Done);
    worker
        .enqueue(PlaybackCommand::Pause, MonotonicTime100ns::new(3))
        .unwrap();
    let pause = next(&mut worker);
    assert_eq!(
        pause.kind(),
        &WorkerActionKind::SetTransport { playing: false }
    );
    complete(&mut worker, &pause, WorkerCompletion::Done);

    worker
        .enqueue(
            PlaybackCommand::Step { frames: -1 },
            MonotonicTime100ns::new(4),
        )
        .unwrap();
    let flush = next(&mut worker);
    complete(&mut worker, &flush, WorkerCompletion::Done);
    let step = next(&mut worker);
    assert!(matches!(
        step.kind(),
        WorkerActionKind::PlanSeek {
            step_frames: Some(-1),
            ..
        }
    ));
    complete(
        &mut worker,
        &step,
        WorkerCompletion::SeekPlanned(plan(at(0, 1), 0)),
    );
    for _ in 0..4 {
        let action = next(&mut worker);
        complete(&mut worker, &action, WorkerCompletion::Done);
    }
    let publish = next(&mut worker);
    complete(
        &mut worker,
        &publish,
        WorkerCompletion::Published { position: at(0, 1) },
    );
    assert_eq!(worker.snapshot().phase, PlaybackPhase::Paused);
    assert_eq!(worker.snapshot().volume, 0.25);

    worker
        .enqueue(
            PlaybackCommand::SetTracks {
                audio_track_indices: vec![1, 2],
            },
            MonotonicTime100ns::new(5),
        )
        .unwrap();
    drive_seek(&mut worker, at(0, 1), 0);
    assert_eq!(worker.snapshot().audio_track_indices, vec![1, 2]);
}

#[test]
fn recoverable_backend_failure_is_revision_fenced_bounded_and_reopenable() {
    let mut worker = PlaybackWorker::new();
    drive_open(&mut worker, "clip.mp4");
    worker.take_events();
    worker
        .enqueue(
            PlaybackCommand::Seek { position: at(3, 1) },
            MonotonicTime100ns::new(100),
        )
        .unwrap();

    let failure = BackendError {
        component: BackendComponent::VideoDecoder,
        kind: BackendErrorKind::DeviceLost,
        recovery: RecoveryDisposition::RecreateComponent,
        native_code: None,
        message: "injected device loss".into(),
    };
    let mut previous_revision = 0;
    for attempt in 0..=MAX_PIPELINE_RECOVERY_ATTEMPTS {
        let action = next(&mut worker);
        if attempt < MAX_PIPELINE_RECOVERY_ATTEMPTS {
            assert!(worker.fail(&action, failure.clone()).unwrap());
            assert!(worker.token().revision() > previous_revision);
            previous_revision = worker.token().revision();
        } else {
            assert!(worker.fail(&action, failure.clone()).unwrap());
        }
    }

    assert_eq!(worker.snapshot().phase, PlaybackPhase::Failed);
    assert!(matches!(
        worker.take_events().as_slice(),
        [PlaybackEvent::Error { message, .. }] if message.contains("injected device loss")
    ));
    assert!(worker.next_action().unwrap().is_none());

    assert_eq!(
        worker
            .enqueue(
                PlaybackCommand::Open {
                    path: PathBuf::from("replacement.mp4"),
                },
                MonotonicTime100ns::new(500),
            )
            .unwrap(),
        EnqueueOutcome::Queued
    );
    drive_open_after_enqueued(&mut worker);
    assert_eq!(worker.snapshot().phase, PlaybackPhase::Paused);
    assert_eq!(worker.recovery_attempts(), 0);
}

fn drive_open_after_enqueued(worker: &mut PlaybackWorker) {
    let indexed = next(worker);
    complete(
        worker,
        &indexed,
        WorkerCompletion::Indexed {
            duration: at(1, 1),
            video_sample_count: 1,
        },
    );
    let flush = next(worker);
    complete(worker, &flush, WorkerCompletion::Done);
    let seek = next(worker);
    complete(
        worker,
        &seek,
        WorkerCompletion::SeekPlanned(plan(at(0, 1), 0)),
    );
    for _ in 0..4 {
        let action = next(worker);
        complete(worker, &action, WorkerCompletion::Done);
    }
    let publish = next(worker);
    complete(
        worker,
        &publish,
        WorkerCompletion::Published { position: at(0, 1) },
    );
}

#[test]
fn indexed_seek_plan_maps_its_exact_target_sample_into_worker_stages() {
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/playback/hybrid-writer-h264-two-opus-5s.mp4");
    let movie = IndexedMovie::open(fixture).unwrap();
    let video_track = movie
        .index()
        .tracks
        .iter()
        .position(|track| matches!(track.config, PlaybackTrackConfig::H264 { .. }))
        .unwrap();
    let audio_tracks: Vec<_> = movie
        .index()
        .tracks
        .iter()
        .enumerate()
        .filter_map(|(index, track)| {
            matches!(track.config, PlaybackTrackConfig::Opus { .. }).then_some(index)
        })
        .collect();
    let indexed = movie
        .seek_plan(video_track, &audio_tracks, at(2_345, 1_000))
        .unwrap();
    let worker = WorkerSeekPlan::try_from(&indexed).unwrap();

    assert_eq!(worker.target, indexed.target_time);
    assert_eq!(
        worker.sync_sample_index,
        indexed.video_sync_sample.sample_index
    );
    assert_eq!(
        worker.target_sample_index,
        indexed.video_preroll.samples.end - 1
    );
}

#[test]
fn steady_progress_and_eof_are_token_fenced_and_ended_emits_once() {
    let mut worker = PlaybackWorker::new();
    drive_open(&mut worker, "clip.mp4");
    worker.take_events();
    worker
        .enqueue(PlaybackCommand::Play, MonotonicTime100ns::new(10))
        .unwrap();
    let play = next(&mut worker);
    complete(&mut worker, &play, WorkerCompletion::Done);
    let token = worker.token();

    assert!(worker.report_position(token, at(5, 1)));
    assert_eq!(worker.snapshot().position, at(5, 1));
    let stale = clipline_playback::PipelineToken::new(
        clipline_playback::WorkGeneration::new(0, 0),
        token.revision(),
    );
    assert!(!worker.report_position(stale, at(6, 1)));
    assert!(!worker.report_ended(stale, at(10, 1)));
    assert!(worker.report_ended(token, at(10, 1)));
    assert!(!worker.report_ended(token, at(10, 1)));
    assert!(!worker.report_position(token, at(9, 1)));

    assert_eq!(worker.snapshot().phase, PlaybackPhase::Ended);
    assert_eq!(worker.snapshot().position, at(10, 1));
    assert!(matches!(
        worker.take_events().as_slice(),
        [PlaybackEvent::Ended { .. }]
    ));
}
