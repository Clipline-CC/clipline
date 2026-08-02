use std::path::PathBuf;

use clipline_playback::{
    CommandError, CommandInbox, EnqueueError, EnqueueOutcome, MonotonicTime100ns, PlaybackCommand,
    PlaybackEvent, PlaybackPhase, PlaybackState, PlaybackTime, WorkGeneration,
    COMMAND_INBOX_CAPACITY, MAX_SELECTED_AUDIO_TRACKS,
};

fn open(path: &str) -> PlaybackCommand {
    PlaybackCommand::Open {
        path: PathBuf::from(path),
    }
}

fn at(ticks: u64, timescale: u32) -> PlaybackTime {
    PlaybackTime::new(ticks, timescale).unwrap()
}

#[test]
fn newer_open_seek_and_close_generations_fence_stale_completions() {
    let mut state = PlaybackState::default();
    assert_eq!(state.snapshot().phase, PlaybackPhase::Closed);
    assert_eq!(state.generation(), WorkGeneration::INITIAL);

    let first_open = state.apply(open("first.mp4")).unwrap();
    assert_eq!(first_open, WorkGeneration::new(1, 0));
    assert_eq!(state.snapshot().phase, PlaybackPhase::Opening);

    let second_open = state.apply(open("second.mp4")).unwrap();
    assert_eq!(second_open, WorkGeneration::new(2, 0));
    assert!(!state.accepts(first_open));
    assert!(state.accepts(second_open));
    assert!(!state.complete_open(first_open, at(10, 1)));
    assert!(state.complete_open(second_open, at(10, 1)));
    assert_eq!(state.snapshot().phase, PlaybackPhase::Paused);

    state.apply(PlaybackCommand::Play).unwrap();
    assert_eq!(state.snapshot().phase, PlaybackPhase::Playing);
    let first_seek = state
        .apply(PlaybackCommand::Seek { position: at(2, 1) })
        .unwrap();
    let second_seek = state
        .apply(PlaybackCommand::Seek { position: at(7, 2) })
        .unwrap();
    assert_eq!(first_seek, WorkGeneration::new(2, 1));
    assert_eq!(second_seek, WorkGeneration::new(2, 2));
    assert!(!state.complete_seek(first_seek, at(2, 1)));
    assert_eq!(state.snapshot().phase, PlaybackPhase::Seeking);
    assert!(state.complete_seek(second_seek, at(7, 2)));
    assert_eq!(state.snapshot().phase, PlaybackPhase::Playing);
    assert_eq!(state.snapshot().position, at(7, 2));

    let closed = state.apply(PlaybackCommand::Close).unwrap();
    assert_eq!(closed, WorkGeneration::new(3, 0));
    assert_eq!(state.snapshot().phase, PlaybackPhase::Closed);
    assert!(!state.accepts(second_seek));
    assert!(matches!(
        PlaybackEvent::Snapshot(state.snapshot()),
        PlaybackEvent::Snapshot(_)
    ));
}

#[test]
fn invalid_commands_fail_without_mutating_state() {
    let mut state = PlaybackState::default();

    for command in [
        PlaybackCommand::Play,
        PlaybackCommand::Pause,
        PlaybackCommand::Seek { position: at(1, 1) },
        PlaybackCommand::Step { frames: 1 },
        PlaybackCommand::SetTracks {
            audio_track_indices: vec![1],
        },
    ] {
        let before = state.snapshot();
        assert_eq!(state.apply(command), Err(CommandError::NoMedia));
        assert_eq!(state.snapshot(), before);
    }

    let before = state.snapshot();
    assert_eq!(state.apply(open("")), Err(CommandError::EmptyPath));
    assert_eq!(state.snapshot(), before);

    for volume in [f32::NAN, -0.01, 1.01] {
        let before = state.snapshot();
        assert_eq!(
            state.apply(PlaybackCommand::SetVolume { volume }),
            Err(CommandError::InvalidVolume)
        );
        assert_eq!(state.snapshot(), before);
    }

    for rate in [f32::NAN, 0.0, -1.0] {
        let before = state.snapshot();
        assert_eq!(
            state.apply(PlaybackCommand::SetRate { rate }),
            Err(CommandError::InvalidRate)
        );
        assert_eq!(state.snapshot(), before);
    }
    assert_eq!(
        state.apply(PlaybackCommand::SetRate { rate: 1.25 }),
        Err(CommandError::UnsupportedRate { milli_rate: 1_250 })
    );

    state.apply(open("clip.mp4")).unwrap();
    let generation = state.generation();
    assert!(state.complete_open(generation, at(5, 1)));
    let before = state.snapshot();
    assert_eq!(
        state.apply(PlaybackCommand::SetTracks {
            audio_track_indices: vec![1, 1],
        }),
        Err(CommandError::DuplicateTrack { track_index: 1 })
    );
    assert_eq!(state.snapshot(), before);
    let too_many: Vec<_> = (0..=MAX_SELECTED_AUDIO_TRACKS).collect();
    assert_eq!(
        state.apply(PlaybackCommand::SetTracks {
            audio_track_indices: too_many,
        }),
        Err(CommandError::TooManyAudioTracks {
            count: MAX_SELECTED_AUDIO_TRACKS + 1,
            limit: MAX_SELECTED_AUDIO_TRACKS,
        })
    );
    assert_eq!(state.snapshot(), before);
    assert_eq!(
        state.apply(PlaybackCommand::Step { frames: 0 }),
        Err(CommandError::InvalidStep)
    );

    let before = state.snapshot();
    assert_eq!(
        state.apply(PlaybackCommand::Seek {
            position: PlaybackTime {
                ticks: 1,
                timescale: 0,
            },
        }),
        Err(CommandError::InvalidTime)
    );
    assert_eq!(state.snapshot(), before);
}

#[test]
fn backend_state_updates_are_generation_and_time_fenced() {
    let mut state = PlaybackState::default();
    let generation = state.apply(open("clip.mp4")).unwrap();
    let forged = PlaybackTime {
        ticks: 10,
        timescale: 0,
    };
    assert!(!state.complete_open(generation, forged));
    assert_eq!(state.snapshot().phase, PlaybackPhase::Opening);
    assert!(state.complete_open(generation, at(10, 1)));

    let stale = WorkGeneration::new(generation.open - 1, generation.seek);
    assert!(!state.update_position(stale, at(2, 1)));
    assert!(!state.update_position(generation, forged));
    assert!(state.update_position(generation, at(2, 1)));
    assert!(state.begin_recovery_seek(generation, at(2, 1)));
    assert!(state.complete_seek(generation, at(2, 1)));
    assert!(state.mark_ended(generation, at(10, 1)));
    assert_eq!(state.snapshot().phase, PlaybackPhase::Ended);
    assert!(!state.fail(stale));
    assert!(state.fail(generation));
    assert_eq!(state.snapshot().phase, PlaybackPhase::Failed);

    for command in [
        PlaybackCommand::Play,
        PlaybackCommand::Pause,
        PlaybackCommand::Seek { position: at(3, 1) },
        PlaybackCommand::Step { frames: 1 },
        PlaybackCommand::SetTracks {
            audio_track_indices: vec![1],
        },
        PlaybackCommand::SetVolume { volume: 0.5 },
        PlaybackCommand::SetRate { rate: 1.0 },
    ] {
        let before = state.snapshot();
        assert_eq!(state.apply(command), Err(CommandError::MediaFailed));
        assert_eq!(state.snapshot(), before);
    }
}

#[test]
fn inbox_coalesces_only_within_the_current_resource_fence() {
    let mut inbox = CommandInbox::new();
    assert_eq!(
        inbox.enqueue(open("one.mp4")).unwrap(),
        EnqueueOutcome::Queued
    );
    assert_eq!(
        inbox.enqueue(PlaybackCommand::Play).unwrap(),
        EnqueueOutcome::Queued
    );
    assert_eq!(
        inbox.enqueue(PlaybackCommand::Pause).unwrap(),
        EnqueueOutcome::Replaced
    );
    assert_eq!(
        inbox
            .enqueue(PlaybackCommand::Seek { position: at(1, 1) })
            .unwrap(),
        EnqueueOutcome::Queued
    );
    assert_eq!(
        inbox
            .enqueue(PlaybackCommand::Seek { position: at(2, 1) })
            .unwrap(),
        EnqueueOutcome::Replaced
    );
    assert_eq!(
        inbox
            .enqueue(PlaybackCommand::SetVolume { volume: 0.5 })
            .unwrap(),
        EnqueueOutcome::Queued
    );
    assert_eq!(
        inbox
            .enqueue(PlaybackCommand::SetVolume { volume: 0.25 })
            .unwrap(),
        EnqueueOutcome::Replaced
    );
    assert_eq!(inbox.len(), 4);
    assert_eq!(inbox.pop_front(), Some(open("one.mp4")));
    assert_eq!(inbox.pop_front(), Some(PlaybackCommand::Pause));
    assert_eq!(
        inbox.pop_front(),
        Some(PlaybackCommand::Seek { position: at(2, 1) })
    );
    assert_eq!(
        inbox.pop_front(),
        Some(PlaybackCommand::SetVolume { volume: 0.25 })
    );

    inbox.enqueue(open("first.mp4")).unwrap();
    inbox.enqueue(PlaybackCommand::Play).unwrap();
    inbox.enqueue(open("second.mp4")).unwrap();
    inbox.enqueue(PlaybackCommand::Pause).unwrap();
    assert_eq!(inbox.len(), 4, "transport intent must not cross Open");
}

#[test]
fn inbox_reserves_capacity_for_close_and_rejects_other_overflow() {
    let mut inbox = CommandInbox::new();
    for index in 0..COMMAND_INBOX_CAPACITY - 1 {
        assert_eq!(
            inbox.enqueue(open(&format!("clip-{index}.mp4"))).unwrap(),
            EnqueueOutcome::Queued
        );
    }
    assert_eq!(inbox.len(), COMMAND_INBOX_CAPACITY - 1);
    assert_eq!(
        inbox.enqueue(PlaybackCommand::Step { frames: 1 }),
        Err(EnqueueError::QueueFull {
            capacity: COMMAND_INBOX_CAPACITY
        })
    );
    assert_eq!(
        inbox.enqueue(PlaybackCommand::Close).unwrap(),
        EnqueueOutcome::Queued
    );
    assert_eq!(inbox.len(), COMMAND_INBOX_CAPACITY);
    assert_eq!(
        inbox.enqueue(PlaybackCommand::Close).unwrap(),
        EnqueueOutcome::Replaced
    );
    assert_eq!(inbox.len(), COMMAND_INBOX_CAPACITY);
}

#[test]
fn coalesced_seek_retains_the_replacement_acceptance_time() {
    let mut inbox = CommandInbox::new();
    inbox
        .enqueue_at(
            PlaybackCommand::Seek { position: at(1, 1) },
            MonotonicTime100ns::new(100),
        )
        .unwrap();
    assert_eq!(
        inbox
            .enqueue_at(
                PlaybackCommand::Seek { position: at(2, 1) },
                MonotonicTime100ns::new(250),
            )
            .unwrap(),
        EnqueueOutcome::Replaced
    );

    let accepted = inbox.pop_front_accepted().unwrap();
    assert_eq!(
        accepted.command,
        PlaybackCommand::Seek { position: at(2, 1) }
    );
    assert_eq!(accepted.accepted_at, MonotonicTime100ns::new(250));
}
