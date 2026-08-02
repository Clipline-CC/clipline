#![cfg(windows)]

use clipline_playback::windows::{
    session_channel, SessionSendError, SessionUpdateError, SessionUpdatePayload,
    UpdatePublishOutcome, SESSION_UPDATE_CAPACITY,
};
use clipline_playback::{
    EnqueueOutcome, MonotonicTime100ns, PipelineToken, PlaybackCommand, PlaybackEvent,
    PlaybackSnapshot, PlaybackState, WorkGeneration, COMMAND_INBOX_CAPACITY,
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
