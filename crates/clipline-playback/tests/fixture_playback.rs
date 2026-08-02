use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use clipline_mp4::{IndexedMovie, PlaybackTrackConfig, SeekPlan};
use clipline_playback::{
    BackendComponent, BackendError, BackendErrorKind, MonotonicTime100ns, PlaybackCommand,
    PlaybackEvent, PlaybackPhase, PlaybackTime, PlaybackWorker, RecoveryDisposition, WorkerAction,
    WorkerActionKind, WorkerCompletion, WorkerSeekPlan,
};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/playback/hybrid-writer-h264-two-opus-5s.mp4")
}

fn at(ticks: u64, timescale: u32) -> PlaybackTime {
    PlaybackTime::new(ticks, timescale).expect("valid playback time")
}

fn same_time(left: PlaybackTime, right: PlaybackTime) -> bool {
    u128::from(left.ticks) * u128::from(right.timescale)
        == u128::from(right.ticks) * u128::from(left.timescale)
}

fn next(worker: &mut PlaybackWorker) -> WorkerAction {
    worker
        .next_action()
        .expect("worker action must be valid")
        .expect("worker must have pending work")
}

/// A deterministic action consumer. The worker owns orchestration while this fake owns the real
/// read-only fixture index, mirroring the resource boundary used by the headless runner.
struct FixtureBackend {
    movie: Option<IndexedMovie<std::fs::File>>,
    video_track_index: Option<usize>,
    last_seek: Option<SeekPlan>,
    last_selected_audio_tracks: Vec<usize>,
}

impl FixtureBackend {
    fn new() -> Self {
        Self {
            movie: None,
            video_track_index: None,
            last_seek: None,
            last_selected_audio_tracks: Vec::new(),
        }
    }

    fn complete(&mut self, worker: &mut PlaybackWorker, action: &WorkerAction) {
        let completion = match action.kind() {
            WorkerActionKind::IndexOpen { path } => {
                self.close();
                let movie = IndexedMovie::open(path).expect("open production-writer fixture");
                let video_track_index = video_track(&movie);
                let index = movie.index();
                let duration = at(index.duration_ticks, index.movie_timescale);
                let video_sample_count = index.tracks[video_track_index].samples.len();
                let default_audio_track_indices = audio_tracks(&movie);
                self.movie = Some(movie);
                self.video_track_index = Some(video_track_index);
                WorkerCompletion::Indexed {
                    duration,
                    video_sample_count,
                    default_audio_track_indices,
                }
            }
            WorkerActionKind::PlanSeek {
                requested,
                audio_track_indices,
                ..
            } => {
                let movie = self.movie.as_ref().expect("fixture index must be open");
                let indexed = movie
                    .seek_plan(
                        self.video_track_index.expect("video track must be known"),
                        audio_track_indices,
                        *requested,
                    )
                    .expect("fixture seek plan");
                let worker_plan = WorkerSeekPlan::try_from(&indexed).expect("worker seek plan");
                self.last_selected_audio_tracks = audio_track_indices.clone();
                self.last_seek = Some(indexed);
                WorkerCompletion::SeekPlanned(worker_plan)
            }
            WorkerActionKind::PublishVideo { .. } => WorkerCompletion::Published {
                position: self
                    .last_seek
                    .as_ref()
                    .expect("seek must precede publication")
                    .target_time,
            },
            WorkerActionKind::CloseBackends => {
                self.close();
                WorkerCompletion::Done
            }
            WorkerActionKind::Flush
            | WorkerActionKind::ReadVideo { .. }
            | WorkerActionKind::ConvertVideo { .. }
            | WorkerActionKind::DecodeVideo { .. }
            | WorkerActionKind::ProduceAudio
            | WorkerActionKind::SetTransport { .. }
            | WorkerActionKind::SetVolume { .. } => WorkerCompletion::Done,
        };
        assert!(
            worker
                .complete(action, completion)
                .expect("complete action"),
            "current action must be accepted"
        );
    }

    fn drive_until_idle(&mut self, worker: &mut PlaybackWorker) {
        let mut actions = 0usize;
        while let Some(action) = worker.next_action().expect("next worker action") {
            self.complete(worker, &action);
            actions += 1;
            assert!(actions < 1_000, "fixture worker did not become idle");
        }
    }

    fn open(&mut self, worker: &mut PlaybackWorker, path: &Path, accepted_at: u64) {
        worker
            .enqueue(
                PlaybackCommand::Open {
                    path: path.to_owned(),
                },
                MonotonicTime100ns::new(accepted_at),
            )
            .expect("queue open");
        self.drive_until_idle(worker);
    }

    fn close(&mut self) {
        self.last_seek = None;
        self.last_selected_audio_tracks.clear();
        self.video_track_index = None;
        self.movie = None;
    }
}

fn video_track(movie: &IndexedMovie<std::fs::File>) -> usize {
    movie
        .index()
        .tracks
        .iter()
        .position(|track| matches!(track.config, PlaybackTrackConfig::H264 { .. }))
        .expect("fixture H.264 track")
}

fn audio_tracks(movie: &IndexedMovie<std::fs::File>) -> Vec<usize> {
    movie
        .index()
        .tracks
        .iter()
        .enumerate()
        .filter_map(|(index, track)| {
            matches!(track.config, PlaybackTrackConfig::Opus { .. }).then_some(index)
        })
        .collect()
}

fn fatal_decoder_error(message: &str) -> BackendError {
    BackendError {
        component: BackendComponent::VideoDecoder,
        kind: BackendErrorKind::DecoderFailure,
        recovery: RecoveryDisposition::Fatal,
        native_code: None,
        message: message.to_owned(),
    }
}

#[test]
fn rapid_distant_seek_storm_only_settles_the_final_generation() {
    let path = fixture_path();
    let mut worker = PlaybackWorker::new();
    let mut backend = FixtureBackend::new();
    backend.open(&mut worker, &path, 1);
    worker.take_events();

    worker
        .enqueue(
            PlaybackCommand::Seek {
                position: at(250, 1_000),
            },
            MonotonicTime100ns::new(10),
        )
        .unwrap();
    let stale_flush = next(&mut worker);

    for (accepted_at, target_ms) in [(20, 4_500), (30, 750), (40, 3_750)] {
        worker
            .enqueue(
                PlaybackCommand::Seek {
                    position: at(target_ms, 1_000),
                },
                MonotonicTime100ns::new(accepted_at),
            )
            .unwrap();
    }

    let final_flush = next(&mut worker);
    assert_ne!(stale_flush.token(), final_flush.token());
    assert!(!worker
        .complete(&stale_flush, WorkerCompletion::Done)
        .unwrap());
    backend.complete(&mut worker, &final_flush);
    backend.drive_until_idle(&mut worker);

    assert!(same_time(worker.snapshot().position, at(3_750, 1_000)));
    assert_eq!(worker.stale_completions(), 1);
    assert!(matches!(
        worker.take_events().as_slice(),
        [PlaybackEvent::SeekSettled { position, .. }] if same_time(*position, at(3_750, 1_000))
    ));
}

#[test]
fn close_during_seek_fences_work_and_releases_the_fixture_owner() {
    let path = fixture_path();
    let mut worker = PlaybackWorker::new();
    let mut backend = FixtureBackend::new();
    backend.open(&mut worker, &path, 1);
    worker.take_events();

    worker
        .enqueue(
            PlaybackCommand::Seek {
                position: at(4_250, 1_000),
            },
            MonotonicTime100ns::new(10),
        )
        .unwrap();
    let stale_flush = next(&mut worker);
    worker
        .enqueue(PlaybackCommand::Close, MonotonicTime100ns::new(11))
        .unwrap();

    let close = next(&mut worker);
    assert_eq!(close.kind(), &WorkerActionKind::CloseBackends);
    assert!(!worker
        .complete(&stale_flush, WorkerCompletion::Done)
        .unwrap());
    backend.complete(&mut worker, &close);

    assert!(backend.movie.is_none());
    assert_eq!(worker.snapshot().phase, PlaybackPhase::Closed);
    assert!(matches!(
        worker.take_events().as_slice(),
        [PlaybackEvent::Closed { .. }]
    ));
}

#[test]
fn fatal_error_can_be_reopened_with_a_fresh_fixture_index() {
    let path = fixture_path();
    let mut worker = PlaybackWorker::new();
    let mut backend = FixtureBackend::new();
    backend.open(&mut worker, &path, 1);
    worker.take_events();

    worker
        .enqueue(
            PlaybackCommand::Seek {
                position: at(2_000, 1_000),
            },
            MonotonicTime100ns::new(10),
        )
        .unwrap();
    let failing = next(&mut worker);
    assert!(worker
        .fail(&failing, fatal_decoder_error("injected fixture failure"))
        .unwrap());
    assert_eq!(worker.snapshot().phase, PlaybackPhase::Failed);
    assert!(matches!(
        worker.take_events().as_slice(),
        [PlaybackEvent::Error { message, .. }] if message == "injected fixture failure"
    ));

    backend.open(&mut worker, &path, 20);
    assert_eq!(worker.snapshot().phase, PlaybackPhase::Paused);
    assert!(same_time(worker.snapshot().position, at(0, 1)));
    assert_eq!(worker.recovery_attempts(), 0);
    assert!(backend.movie.is_some());
    assert!(matches!(
        worker.take_events().as_slice(),
        [PlaybackEvent::Opened { .. }]
    ));
}

#[test]
fn track_switch_during_playback_reseeks_at_position_and_preserves_playing_intent() {
    let path = fixture_path();
    let mut worker = PlaybackWorker::new();
    let mut backend = FixtureBackend::new();
    backend.open(&mut worker, &path, 1);
    worker.take_events();
    let selected = audio_tracks(backend.movie.as_ref().unwrap());
    assert_eq!(
        selected.len(),
        2,
        "production fixture must contain two Opus tracks"
    );
    assert_eq!(worker.snapshot().audio_track_indices, selected);

    worker
        .enqueue(PlaybackCommand::Play, MonotonicTime100ns::new(10))
        .unwrap();
    backend.drive_until_idle(&mut worker);
    let position = at(2_250, 1_000);
    assert!(worker.report_position(worker.token(), position));

    worker
        .enqueue(
            PlaybackCommand::SetTracks {
                audio_track_indices: vec![selected[0]],
            },
            MonotonicTime100ns::new(20),
        )
        .unwrap();
    backend.drive_until_idle(&mut worker);

    let indexed = backend.last_seek.as_ref().expect("track switch seek plan");
    assert_eq!(backend.last_selected_audio_tracks, vec![selected[0]]);
    assert_eq!(indexed.audio_preroll.len(), 1);
    assert!(indexed
        .audio_preroll
        .iter()
        .all(|range| !range.samples.is_empty()));
    assert_eq!(worker.snapshot().phase, PlaybackPhase::Playing);
    assert!(worker.snapshot().playing_intent);
    assert!(same_time(worker.snapshot().position, position));
    assert!(matches!(
        worker.take_events().as_slice(),
        [PlaybackEvent::SeekSettled { position: settled, .. }] if same_time(*settled, position)
    ));
}

#[test]
fn stale_completion_from_an_old_open_cannot_mutate_the_replacement() {
    let path = fixture_path();
    let mut worker = PlaybackWorker::new();
    let mut backend = FixtureBackend::new();
    worker
        .enqueue(
            PlaybackCommand::Open { path: path.clone() },
            MonotonicTime100ns::new(1),
        )
        .unwrap();
    let stale_open = next(&mut worker);

    worker
        .enqueue(
            PlaybackCommand::Open { path: path.clone() },
            MonotonicTime100ns::new(2),
        )
        .unwrap();
    let replacement_open = next(&mut worker);
    assert_ne!(stale_open.token(), replacement_open.token());
    let before = worker.snapshot();
    assert!(!worker
        .complete(
            &stale_open,
            WorkerCompletion::Indexed {
                duration: at(99, 1),
                video_sample_count: 1,
                default_audio_track_indices: Vec::new(),
            },
        )
        .unwrap());
    assert_eq!(worker.snapshot(), before);
    assert_eq!(worker.stale_completions(), 1);

    backend.complete(&mut worker, &replacement_open);
    backend.drive_until_idle(&mut worker);
    assert_eq!(worker.snapshot().phase, PlaybackPhase::Paused);
    assert!(!worker
        .fail(&stale_open, fatal_decoder_error("late failure"))
        .unwrap());
    assert_eq!(worker.stale_completions(), 2);
    assert!(matches!(
        worker.take_events().as_slice(),
        [PlaybackEvent::Opened { .. }]
    ));
}

#[test]
fn production_fixture_seek_plan_drives_exact_worker_sample_range() {
    let path = fixture_path();
    let movie = IndexedMovie::open(&path).expect("open fixture");
    let video_track_index = video_track(&movie);
    let selected = audio_tracks(&movie);
    let indexed = movie
        .seek_plan(video_track_index, &selected, at(2_345, 1_000))
        .expect("indexed seek plan");
    let worker_plan = WorkerSeekPlan::try_from(&indexed).expect("worker seek plan");

    assert_eq!(worker_plan.target, indexed.target_time);
    assert_eq!(
        worker_plan.sync_sample_index,
        indexed.video_preroll.samples.start
    );
    assert_eq!(
        worker_plan.target_sample_index,
        indexed.video_preroll.samples.end - 1
    );
    assert_eq!(indexed.audio_preroll.len(), selected.len());
    assert!(indexed
        .audio_preroll
        .iter()
        .zip(selected)
        .all(|(range, track_index)| range.track_index == track_index));
}

struct TempFixture {
    directory: PathBuf,
    source: PathBuf,
    renamed: PathBuf,
}

impl TempFixture {
    fn copy() -> Self {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "clipline-fixture-playback-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&directory).expect("create isolated fixture directory");
        let source = directory.join("open.mp4");
        let renamed = directory.join("closed.mp4");
        fs::copy(fixture_path(), &source).expect("copy production fixture");
        Self {
            directory,
            source,
            renamed,
        }
    }
}

impl Drop for TempFixture {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.source);
        let _ = fs::remove_file(&self.renamed);
        let _ = fs::remove_dir(&self.directory);
    }
}

#[test]
fn fixture_can_be_renamed_immediately_after_index_owner_closes() {
    let fixture = TempFixture::copy();
    let movie = IndexedMovie::open(&fixture.source).expect("open copied fixture");
    assert!(!movie.index().tracks.is_empty());

    drop(movie);
    fs::rename(&fixture.source, &fixture.renamed)
        .expect("closed IndexedMovie must not retain the input file handle");
    assert!(fixture.renamed.is_file());
}
