#![cfg(windows)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use clipline_library::LocalLibraryRepository;
use clipline_playback::windows::{session_channel, SessionUpdate, SessionUpdatePayload};
use clipline_playback::{
    PipelineToken, PlaybackCommand, PlaybackPhase, PlaybackSnapshot, PlaybackTime, WorkGeneration,
    COMMAND_INBOX_CAPACITY,
};
use clipline_slint_spike::controller::PlaybackCommandPort;
use clipline_slint_spike::live::{
    LiveMediaCommandPort, LiveMediaLease, LiveMediaOpenOutcome, LiveMediaRequestToken,
    SessionCommandPort, ValidatedLiveMediaSource, MAX_PENDING_LIVE_MEDIA_OPENS,
};
use clipline_test_utils::TestDir;

struct DropProbe(Arc<AtomicUsize>);

impl Drop for DropProbe {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

fn local_source(
    directory: &TestDir,
    name: &str,
    drops: Arc<AtomicUsize>,
) -> (PathBuf, ValidatedLiveMediaSource) {
    let root = directory.path().join("media");
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join(name);
    std::fs::write(&path, b"not opened by the ownership test").unwrap();
    let source = LocalLibraryRepository::open(&root)
        .unwrap()
        .validate_clip_path(path.to_string_lossy().as_ref())
        .unwrap();
    let canonical = source.canonical_path().to_path_buf();
    (
        canonical,
        ValidatedLiveMediaSource::local(source, LiveMediaLease::new(DropProbe(drops))),
    )
}

fn snapshot(open: u64, phase: PlaybackPhase, path: Option<PathBuf>) -> SessionUpdate {
    let generation = WorkGeneration::new(open, 0);
    SessionUpdate {
        sequence: open,
        token: PipelineToken::new(generation, 0),
        payload: SessionUpdatePayload::Snapshot(PlaybackSnapshot {
            phase,
            generation,
            path,
            position: PlaybackTime::new(0, 1).unwrap(),
            duration: None,
            audio_track_indices: Vec::new(),
            volume: 1.0,
            rate: 1.0,
            playing_intent: false,
        }),
    }
}

#[test]
fn dynamic_open_is_bounded_and_rejects_stale_incoming_leases() {
    let directory = TestDir::new("clipline-slint-spike", "live-open-bound");
    let drops = Arc::new(AtomicUsize::new(0));
    let (client, mut runtime) = session_channel();
    let port = LiveMediaCommandPort::new_dynamic(Arc::new(client));

    for request in 1..=u64::try_from(MAX_PENDING_LIVE_MEDIA_OPENS).unwrap() {
        let (path, source) = local_source(
            &directory,
            &format!("clip-{request}.mp4"),
            Arc::clone(&drops),
        );
        assert_eq!(
            port.open(LiveMediaRequestToken::new(request).unwrap(), source)
                .unwrap(),
            LiveMediaOpenOutcome::Accepted {
                playback_open_generation: request,
            }
        );
        assert_eq!(
            runtime.try_recv_command().unwrap().command,
            PlaybackCommand::Open { path }
        );
    }

    let (_, overflow) = local_source(&directory, "overflow.mp4", Arc::clone(&drops));
    assert!(port
        .open(
            LiveMediaRequestToken::new(u64::try_from(MAX_PENDING_LIVE_MEDIA_OPENS).unwrap() + 1,)
                .unwrap(),
            overflow,
        )
        .unwrap_err()
        .to_string()
        .contains("full"));
    assert_eq!(drops.load(Ordering::SeqCst), 1);

    let (_, stale) = local_source(&directory, "stale.mp4", Arc::clone(&drops));
    assert_eq!(
        port.open(LiveMediaRequestToken::new(1).unwrap(), stale)
            .unwrap(),
        LiveMediaOpenOutcome::Stale
    );
    assert_eq!(drops.load(Ordering::SeqCst), 2);

    port.release_all_after_backend_shutdown();
    assert_eq!(
        drops.load(Ordering::SeqCst),
        MAX_PENDING_LIVE_MEDIA_OPENS + 2
    );
}

#[test]
fn replacement_releases_the_prior_lease_only_at_ready_publication() {
    let directory = TestDir::new("clipline-slint-spike", "live-replace-order");
    let first_drops = Arc::new(AtomicUsize::new(0));
    let second_drops = Arc::new(AtomicUsize::new(0));
    let (client, mut runtime) = session_channel();
    let port = LiveMediaCommandPort::new_dynamic(Arc::new(client));

    let (first_path, first) = local_source(&directory, "first.mp4", Arc::clone(&first_drops));
    port.open(LiveMediaRequestToken::new(1).unwrap(), first)
        .unwrap();
    assert!(matches!(
        runtime.try_recv_command().unwrap().command,
        PlaybackCommand::Open { .. }
    ));
    port.accept_session_update(&snapshot(0, PlaybackPhase::Closed, None));
    assert_eq!(
        first_drops.load(Ordering::SeqCst),
        0,
        "the runtime's initial Closed snapshot must not release a future open"
    );
    port.accept_session_update(&snapshot(1, PlaybackPhase::Paused, Some(first_path)));
    assert_eq!(first_drops.load(Ordering::SeqCst), 0);

    let (second_path, second) = local_source(&directory, "second.mp4", Arc::clone(&second_drops));
    port.open(LiveMediaRequestToken::new(2).unwrap(), second)
        .unwrap();
    assert!(matches!(
        runtime.try_recv_command().unwrap().command,
        PlaybackCommand::Open { .. }
    ));
    port.accept_session_update(&snapshot(
        2,
        PlaybackPhase::Opening,
        Some(second_path.clone()),
    ));
    assert_eq!(first_drops.load(Ordering::SeqCst), 0);
    assert_eq!(second_drops.load(Ordering::SeqCst), 0);

    // A ready snapshot is produced only after IndexOpen has closed the prior
    // file/decoder/audio resources. The port consumes it before the UI pump.
    port.accept_session_update(&snapshot(2, PlaybackPhase::Paused, Some(second_path)));
    assert_eq!(first_drops.load(Ordering::SeqCst), 1);
    assert_eq!(second_drops.load(Ordering::SeqCst), 0);

    assert!(port.close().unwrap());
    assert_eq!(
        runtime.try_recv_command().unwrap().command,
        PlaybackCommand::Close
    );
    assert_eq!(second_drops.load(Ordering::SeqCst), 0);
    port.accept_session_update(&snapshot(3, PlaybackPhase::Closed, None));
    assert_eq!(second_drops.load(Ordering::SeqCst), 1);
}

#[test]
fn a_superseded_open_never_retains_its_source_lease() {
    let directory = TestDir::new("clipline-slint-spike", "live-superseded-open");
    let first_drops = Arc::new(AtomicUsize::new(0));
    let second_drops = Arc::new(AtomicUsize::new(0));
    let (client, _runtime) = session_channel();
    let port = LiveMediaCommandPort::new_dynamic(Arc::new(client));

    let (_, first) = local_source(&directory, "first.mp4", Arc::clone(&first_drops));
    let (second_path, second) = local_source(&directory, "second.mp4", Arc::clone(&second_drops));
    port.open(LiveMediaRequestToken::new(1).unwrap(), first)
        .unwrap();
    port.open(LiveMediaRequestToken::new(2).unwrap(), second)
        .unwrap();

    // The playback worker may drain both resource fences before executing.
    // Only generation 2 becomes ready; generation 1 is therefore stale.
    port.accept_session_update(&snapshot(2, PlaybackPhase::Paused, Some(second_path)));
    assert_eq!(first_drops.load(Ordering::SeqCst), 1);
    assert_eq!(second_drops.load(Ordering::SeqCst), 0);

    port.accept_session_update(&snapshot(1, PlaybackPhase::Failed, None));
    assert_eq!(
        second_drops.load(Ordering::SeqCst),
        0,
        "an out-of-order terminal snapshot cannot release current media"
    );

    port.release_all_after_backend_shutdown();
    assert_eq!(second_drops.load(Ordering::SeqCst), 1);
}

#[test]
fn terminal_close_uses_the_reserved_inbox_slot_and_disconnect_is_a_safe_fallback() {
    let (client, mut runtime) = session_channel();
    let client = Arc::new(client);
    for index in 0..(COMMAND_INBOX_CAPACITY - 1) {
        client
            .try_send(PlaybackCommand::Open {
                path: PathBuf::from(format!("queued-{index}.mp4")),
            })
            .unwrap();
    }
    let port = LiveMediaCommandPort::new_dynamic(Arc::clone(&client));
    assert!(port.close_or_disconnect());

    let mut commands = Vec::new();
    while let Some(command) = runtime.try_recv_command() {
        commands.push(command.command);
    }
    assert_eq!(commands.len(), COMMAND_INBOX_CAPACITY);
    assert_eq!(commands.last(), Some(&PlaybackCommand::Close));

    client.disconnect();
    assert!(client.try_send(PlaybackCommand::Pause).is_err());
}

#[test]
fn controller_command_port_cannot_bypass_live_media_ownership() {
    let (client, mut runtime) = session_channel();
    let port = SessionCommandPort::new(Arc::new(client));

    assert!(port
        .send(PlaybackCommand::Open {
            path: PathBuf::from("unfenced.mp4"),
        })
        .unwrap_err()
        .contains("lease-owning"));
    assert!(port.send(PlaybackCommand::Close).is_err());
    assert!(runtime.try_recv_command().is_none());

    port.send(PlaybackCommand::Pause).unwrap();
    assert_eq!(
        runtime.try_recv_command().unwrap().command,
        PlaybackCommand::Pause
    );
}
