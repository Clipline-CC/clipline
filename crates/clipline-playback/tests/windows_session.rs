#![cfg(windows)]

use std::fs;
use std::thread;
use std::time::{Duration, Instant};

use clipline_playback::windows::{
    session_channel, D3D11VideoSurface, SessionExit, SessionSendError, SessionUpdateError,
    SessionUpdatePayload, UpdatePublishOutcome, SESSION_UPDATE_CAPACITY,
};
use clipline_playback::{
    DecodedVideoFrame, EnqueueOutcome, FramePublisher, MonotonicTime100ns, PipelineToken,
    PlaybackCommand, PlaybackEvent, PlaybackPhase, PlaybackSnapshot, PlaybackState,
    PublicationReceipt, WorkGeneration, COMMAND_INBOX_CAPACITY,
};

fn token(revision: u64) -> PipelineToken {
    PipelineToken::new(WorkGeneration::new(1, 0), revision)
}

fn snapshot() -> PlaybackSnapshot {
    PlaybackState::default().snapshot()
}

#[test]
fn command_port_coalesces_intent_reserves_close_and_reports_disconnect() {
    let (client, mut runtime) = session_channel();
    assert_eq!(
        client
            .try_send_at(PlaybackCommand::Play, MonotonicTime100ns::new(1))
            .unwrap(),
        EnqueueOutcome::Queued
    );
    assert_eq!(
        client
            .try_send_at(PlaybackCommand::Pause, MonotonicTime100ns::new(2))
            .unwrap(),
        EnqueueOutcome::Replaced
    );
    assert!(matches!(
        runtime.try_recv_command().unwrap().command,
        PlaybackCommand::Pause
    ));

    for _ in 0..COMMAND_INBOX_CAPACITY - 1 {
        assert_eq!(
            client
                .try_send(PlaybackCommand::Step { frames: 1 })
                .unwrap(),
            EnqueueOutcome::Queued
        );
    }
    assert_eq!(
        client.try_send(PlaybackCommand::Play),
        Err(SessionSendError::Full {
            capacity: COMMAND_INBOX_CAPACITY,
        })
    );
    assert_eq!(
        client.try_send(PlaybackCommand::Close).unwrap(),
        EnqueueOutcome::Queued
    );
    let commands: Vec<_> = std::iter::from_fn(|| runtime.try_recv_command()).collect();
    assert_eq!(commands.len(), COMMAND_INBOX_CAPACITY);
    assert!(matches!(
        commands.last().unwrap().command,
        PlaybackCommand::Close
    ));

    drop(runtime);
    assert_eq!(
        client.try_send(PlaybackCommand::Play),
        Err(SessionSendError::Disconnected)
    );
}

#[test]
fn update_sink_is_owned_revisioned_coalesced_and_bounded() {
    let (client, mut runtime) = session_channel();
    for revision in 0..100 {
        let outcome = runtime
            .publish_update(token(revision), SessionUpdatePayload::Snapshot(snapshot()))
            .unwrap();
        assert_eq!(
            outcome,
            if revision == 0 {
                UpdatePublishOutcome::Queued
            } else {
                UpdatePublishOutcome::Replaced
            }
        );
    }
    let latest = client.try_recv_update().unwrap();
    assert_eq!(latest.token, token(99));
    assert_eq!(latest.sequence, 100);
    assert!(matches!(latest.payload, SessionUpdatePayload::Snapshot(_)));
    assert!(client.try_recv_update().is_none());

    for sequence in 0..SESSION_UPDATE_CAPACITY - 1 {
        runtime
            .publish_update(
                token(sequence as u64),
                SessionUpdatePayload::Event(PlaybackEvent::Opened {
                    generation: WorkGeneration::new(1, sequence as u64),
                    duration: clipline_playback::PlaybackTime::new(1, 1).unwrap(),
                }),
            )
            .unwrap();
    }
    assert_eq!(
        runtime.publish_update(
            token(999),
            SessionUpdatePayload::Event(PlaybackEvent::Opened {
                generation: WorkGeneration::new(2, 0),
                duration: clipline_playback::PlaybackTime::new(1, 1).unwrap(),
            }),
        ),
        Err(SessionUpdateError::Full {
            capacity: SESSION_UPDATE_CAPACITY,
        })
    );
    assert_eq!(
        runtime
            .publish_update(
                token(1_000),
                SessionUpdatePayload::Event(PlaybackEvent::Closed {
                    generation: WorkGeneration::new(2, 0),
                }),
            )
            .unwrap(),
        UpdatePublishOutcome::Queued
    );
    assert_eq!(
        runtime.publish_update(
            token(1_001),
            SessionUpdatePayload::Event(PlaybackEvent::Closed {
                generation: WorkGeneration::new(2, 0),
            }),
        ),
        Err(SessionUpdateError::Full {
            capacity: SESSION_UPDATE_CAPACITY,
        })
    );

    drop(client);
    assert_eq!(
        runtime.publish_update(
            token(1_002),
            SessionUpdatePayload::Event(PlaybackEvent::Error {
                generation: WorkGeneration::new(2, 0),
                message: "owned message".to_owned(),
            }),
        ),
        Err(SessionUpdateError::Disconnected)
    );
}

#[derive(Default)]
struct DropPublisher;

impl FramePublisher<D3D11VideoSurface> for DropPublisher {
    fn publish(
        &mut self,
        frame: DecodedVideoFrame<D3D11VideoSurface>,
    ) -> Result<PublicationReceipt, clipline_playback::BackendError> {
        drop(frame);
        Ok(PublicationReceipt::Presented)
    }

    fn clear(&mut self, _token: PipelineToken) -> Result<(), clipline_playback::BackendError> {
        Ok(())
    }
}

#[derive(Default)]
struct BackpressuredPublisher;

impl FramePublisher<D3D11VideoSurface> for BackpressuredPublisher {
    fn publish(
        &mut self,
        frame: DecodedVideoFrame<D3D11VideoSurface>,
    ) -> Result<PublicationReceipt, clipline_playback::BackendError> {
        drop(frame);
        Ok(PublicationReceipt::Backpressured)
    }

    fn clear(&mut self, _token: PipelineToken) -> Result<(), clipline_playback::BackendError> {
        Ok(())
    }
}

#[test]
fn live_session_routes_worker_commands_and_releases_every_backend_on_close() {
    if std::env::var_os("CI").is_some() {
        eprintln!("SKIP: Windows live playback session device test is disabled under CI");
        return;
    }

    let source = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/playback/hybrid-writer-h264-two-opus-5s.mp4");
    let directory =
        std::env::temp_dir().join(format!("clipline-live-session-test-{}", std::process::id()));
    fs::create_dir_all(&directory).unwrap();
    let active = directory.join("active.mp4");
    let renamed = directory.join("released.mp4");
    let _ = fs::remove_file(&active);
    let _ = fs::remove_file(&renamed);
    fs::copy(source, &active).unwrap();

    let (client, runtime) = session_channel();
    let playback = thread::Builder::new()
        .name("clipline-live-session-test".into())
        .spawn(move || runtime.run(DropPublisher))
        .unwrap();
    client
        .try_send(PlaybackCommand::Open {
            path: active.clone(),
        })
        .unwrap();

    let mut deadline = Instant::now() + Duration::from_secs(10);
    let mut selected_tracks = Vec::new();
    let mut opened = false;
    while Instant::now() < deadline && !opened {
        while let Some(update) = client.try_recv_update() {
            match update.payload {
                SessionUpdatePayload::Snapshot(snapshot) => {
                    selected_tracks = snapshot.audio_track_indices;
                }
                SessionUpdatePayload::Event(PlaybackEvent::Opened { .. }) => opened = true,
                SessionUpdatePayload::Event(PlaybackEvent::Error { message, .. }) => {
                    client.try_send(PlaybackCommand::Close).unwrap();
                    let exit = playback.join().unwrap().unwrap();
                    assert_eq!(exit, SessionExit::Closed);
                    let _ = fs::remove_file(&active);
                    let _ = fs::remove_dir(&directory);
                    eprintln!("SKIP: live playback session devices are unavailable: {message}");
                    return;
                }
                SessionUpdatePayload::Event(_) | SessionUpdatePayload::Metrics(_) => {}
            }
        }
        thread::sleep(Duration::from_millis(2));
    }
    assert!(
        opened,
        "live session did not open within the device timeout"
    );
    assert_eq!(selected_tracks.len(), 2);

    client.try_send(PlaybackCommand::Play).unwrap();
    let seek_target = clipline_playback::PlaybackTime::new(2_345, 1_000).unwrap();
    client
        .try_send(PlaybackCommand::Seek {
            position: seek_target,
        })
        .unwrap();
    let mut settled = None;
    let mut seek_error = None;
    deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && settled.is_none() {
        while let Some(update) = client.try_recv_update() {
            match update.payload {
                SessionUpdatePayload::Event(PlaybackEvent::SeekSettled { position, .. }) => {
                    settled = Some(position)
                }
                SessionUpdatePayload::Event(PlaybackEvent::Error { message, .. }) => {
                    seek_error = Some(message)
                }
                _ => {}
            }
        }
        thread::sleep(Duration::from_millis(2));
    }
    if settled.is_none() && playback.is_finished() {
        let result = playback.join().unwrap();
        panic!("live session runtime exited before seek settled: {result:?}");
    }
    assert!(
        settled.is_some(),
        "live session seek did not settle; error={seek_error:?}"
    );

    client
        .try_send(PlaybackCommand::Step { frames: 1 })
        .unwrap();
    let mut stepped = None;
    deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && stepped.is_none() {
        while let Some(update) = client.try_recv_update() {
            if let SessionUpdatePayload::Event(PlaybackEvent::SeekSettled { position, .. }) =
                update.payload
            {
                stepped = Some(position);
            }
        }
        thread::sleep(Duration::from_millis(2));
    }
    let settled = settled.unwrap();
    let stepped = stepped.expect("exact frame step did not settle");
    assert!(
        u128::from(stepped.ticks) * u128::from(settled.timescale)
            > u128::from(settled.ticks) * u128::from(stepped.timescale)
    );

    client
        .try_send(PlaybackCommand::SetTracks {
            audio_track_indices: vec![selected_tracks[0]],
        })
        .unwrap();
    client
        .try_send(PlaybackCommand::SetVolume { volume: 0.25 })
        .unwrap();
    client.try_send(PlaybackCommand::Pause).unwrap();
    let mut final_snapshot = None;
    deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        while let Some(update) = client.try_recv_update() {
            if let SessionUpdatePayload::Snapshot(snapshot) = update.payload {
                if snapshot.phase == PlaybackPhase::Paused
                    && snapshot.audio_track_indices == vec![selected_tracks[0]]
                    && snapshot.volume.to_bits() == 0.25_f32.to_bits()
                {
                    final_snapshot = Some(snapshot);
                    break;
                }
            }
        }
        if final_snapshot.is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(2));
    }
    assert!(final_snapshot.is_some(), "live controls did not settle");

    client.try_send(PlaybackCommand::Close).unwrap();
    assert_eq!(playback.join().unwrap().unwrap(), SessionExit::Closed);
    fs::rename(&active, &renamed).expect("session close must release the indexed fixture");
    fs::rename(&renamed, &active).unwrap();

    let (disconnecting_client, disconnecting_runtime) = session_channel();
    let disconnected_playback = thread::Builder::new()
        .name("clipline-disconnected-session-test".into())
        .spawn(move || disconnecting_runtime.run(DropPublisher))
        .unwrap();
    disconnecting_client
        .try_send(PlaybackCommand::Open {
            path: active.clone(),
        })
        .unwrap();
    deadline = Instant::now() + Duration::from_secs(10);
    let mut reopened = false;
    while Instant::now() < deadline && !reopened {
        while let Some(update) = disconnecting_client.try_recv_update() {
            if matches!(
                update.payload,
                SessionUpdatePayload::Event(PlaybackEvent::Opened { .. })
            ) {
                reopened = true;
            }
        }
        thread::sleep(Duration::from_millis(2));
    }
    assert!(reopened, "disconnect teardown session did not open");
    drop(disconnecting_client);
    assert_eq!(
        disconnected_playback.join().unwrap().unwrap(),
        SessionExit::ClientDisconnected
    );
    fs::rename(&active, &renamed).expect("client disconnect must release the indexed fixture");
    fs::rename(&renamed, &active).unwrap();
    fs::remove_file(&active).unwrap();
    fs::remove_dir(&directory).unwrap();
}

#[test]
fn repeated_publication_backpressure_exhausts_recovery_and_releases_media() {
    if std::env::var_os("CI").is_some() {
        eprintln!("SKIP: Windows live playback session device test is disabled under CI");
        return;
    }

    let source = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/playback/hybrid-writer-h264-two-opus-5s.mp4");
    let directory = std::env::temp_dir().join(format!(
        "clipline-backpressured-session-test-{}",
        std::process::id()
    ));
    fs::create_dir_all(&directory).unwrap();
    let active = directory.join("active.mp4");
    let released = directory.join("released.mp4");
    let _ = fs::remove_file(&active);
    let _ = fs::remove_file(&released);
    fs::copy(source, &active).unwrap();

    let (client, runtime) = session_channel();
    let playback = thread::Builder::new()
        .name("clipline-backpressured-session-test".into())
        .spawn(move || runtime.run(BackpressuredPublisher))
        .unwrap();
    client
        .try_send(PlaybackCommand::Open {
            path: active.clone(),
        })
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut failure = None;
    while Instant::now() < deadline && failure.is_none() {
        while let Some(update) = client.try_recv_update() {
            if let SessionUpdatePayload::Event(PlaybackEvent::Error { message, .. }) =
                update.payload
            {
                failure = Some(message);
            }
        }
        thread::sleep(Duration::from_millis(2));
    }
    let failure = failure.expect("bounded publication recovery did not become terminal");
    assert!(
        failure.contains("backpressured"),
        "unexpected error: {failure}"
    );
    fs::rename(&active, &released).expect("terminal failure must release the indexed fixture");
    fs::rename(&released, &active).unwrap();

    client.try_send(PlaybackCommand::Close).unwrap();
    assert_eq!(playback.join().unwrap().unwrap(), SessionExit::Closed);
    fs::remove_file(&active).unwrap();
    fs::remove_dir(&directory).unwrap();
}
