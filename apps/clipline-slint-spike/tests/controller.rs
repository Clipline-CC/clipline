use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use clipline_playback::windows::{SessionUpdate, SessionUpdatePayload};
use clipline_playback::{
    PipelineToken, PlaybackCommand, PlaybackEvent, PlaybackPhase, PlaybackSnapshot, PlaybackTime,
    WorkGeneration, PLAYBACK_TIMELINE_HZ,
};
use clipline_slint_spike::controller::{
    ApplyUpdateOutcome, PlaybackCommandPort, PlaybackController, ShutdownOrder, ShutdownStage,
};

#[derive(Clone, Default)]
struct FakePort {
    commands: Arc<Mutex<Vec<PlaybackCommand>>>,
}

impl PlaybackCommandPort for FakePort {
    fn send(&self, command: PlaybackCommand) -> Result<(), String> {
        self.commands.lock().unwrap().push(command);
        Ok(())
    }
}

fn token(open: u64, seek: u64, revision: u64) -> PipelineToken {
    PipelineToken::new(WorkGeneration::new(open, seek), revision)
}

fn snapshot(generation: WorkGeneration, playing: bool) -> PlaybackSnapshot {
    PlaybackSnapshot {
        phase: if playing {
            PlaybackPhase::Playing
        } else {
            PlaybackPhase::Paused
        },
        generation,
        path: Some(PathBuf::from("fixture.mp4")),
        position: PlaybackTime::new(5 * PLAYBACK_TIMELINE_HZ as u64, PLAYBACK_TIMELINE_HZ).unwrap(),
        duration: Some(
            PlaybackTime::new(10 * PLAYBACK_TIMELINE_HZ as u64, PLAYBACK_TIMELINE_HZ).unwrap(),
        ),
        audio_track_indices: vec![2, 3],
        volume: 0.8,
        rate: 1.0,
        playing_intent: playing,
    }
}

#[test]
fn callbacks_map_to_owned_playback_commands() {
    let port = FakePort::default();
    let commands = Arc::clone(&port.commands);
    let mut controller = PlaybackController::new(port);
    controller.apply_update(SessionUpdate {
        sequence: 1,
        token: token(1, 0, 1),
        payload: SessionUpdatePayload::Snapshot(snapshot(WorkGeneration::new(1, 0), false)),
    });

    controller.open(PathBuf::from("next.mp4")).unwrap();
    controller.play_pause().unwrap();
    controller.seek_relative(-7.0).unwrap();
    controller.set_track(2, false).unwrap();
    controller.set_track(7, true).unwrap();
    controller.set_volume(0.25).unwrap();
    controller.close().unwrap();

    assert_eq!(
        *commands.lock().unwrap(),
        vec![
            PlaybackCommand::Open {
                path: PathBuf::from("next.mp4")
            },
            PlaybackCommand::Play,
            PlaybackCommand::Seek {
                position: PlaybackTime::new(0, PLAYBACK_TIMELINE_HZ).unwrap()
            },
            PlaybackCommand::SetTracks {
                audio_track_indices: vec![3]
            },
            PlaybackCommand::SetTracks {
                audio_track_indices: vec![2, 3, 7]
            },
            PlaybackCommand::SetVolume { volume: 0.25 },
            PlaybackCommand::Close,
        ]
    );
}

#[test]
fn newer_revisions_apply_and_late_ui_work_is_rejected() {
    let mut controller = PlaybackController::new(FakePort::default());
    let current = token(2, 4, 9);
    assert_eq!(
        controller.apply_update(SessionUpdate {
            sequence: 10,
            token: current,
            payload: SessionUpdatePayload::Snapshot(snapshot(current.work(), true)),
        }),
        ApplyUpdateOutcome::Applied
    );
    assert!(controller.ui_state().playing);

    assert_eq!(
        controller.apply_update(SessionUpdate {
            sequence: 11,
            token: token(2, 3, 99),
            payload: SessionUpdatePayload::Event(PlaybackEvent::Error {
                generation: WorkGeneration::new(2, 3),
                message: "stale".into(),
            }),
        }),
        ApplyUpdateOutcome::IgnoredStale
    );
    assert_eq!(controller.latest_token(), Some(current));
    assert!(controller.ui_state().playing);

    assert_eq!(
        controller.apply_update(SessionUpdate {
            sequence: 10,
            token: token(2, 4, 10),
            payload: SessionUpdatePayload::Event(PlaybackEvent::Error {
                generation: WorkGeneration::new(2, 4),
                message: "out of order".into(),
            }),
        }),
        ApplyUpdateOutcome::IgnoredStale
    );
}

#[test]
fn errors_are_owned_for_display_and_shutdown_order_is_enforced() {
    let mut controller = PlaybackController::new(FakePort::default());
    controller.apply_update(SessionUpdate {
        sequence: 1,
        token: token(1, 0, 1),
        payload: SessionUpdatePayload::Event(PlaybackEvent::Error {
            generation: WorkGeneration::new(1, 0),
            message: "device removed".to_owned(),
        }),
    });
    assert_eq!(controller.ui_state().phase, PlaybackPhase::Failed);
    assert_eq!(
        controller.ui_state().status,
        "Playback error: device removed"
    );

    let mut shutdown = ShutdownOrder::default();
    assert!(shutdown.host_destroyed().is_err());
    shutdown.session_stopped().unwrap();
    shutdown.host_destroyed().unwrap();
    shutdown.ui_dropped().unwrap();
    assert_eq!(shutdown.stage(), ShutdownStage::UiDropped);
}
