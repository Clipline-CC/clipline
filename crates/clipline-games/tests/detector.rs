use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use clipline_games::detection::DetectedGame;
use clipline_games::detector::{
    GameDetectionEvent, GameDetectionProbe, GameDetectionSink, GameDetectorCheckpoint,
    GameDetectorService, GameDetectorServiceError, GameDetectorSinkError,
    GameDetectorThreadSpawner, GameDetectorToken, RejectedGameDetectionEvent,
};
use clipline_settings::games::{GameRecordingMode, GameSettings};

fn config(marker: usize) -> GameSettings {
    let mut settings = GameSettings::default();
    settings
        .custom_games
        .push(clipline_settings::CustomGameSettings {
            id: format!("custom-marker-{marker}"),
            legacy_ids: Vec::new(),
            name: format!("Marker {marker}"),
            enabled: true,
            exe_name: format!("marker-{marker}.exe"),
            process_path: None,
            window_title: format!("Marker {marker}"),
            recording_mode: GameRecordingMode::ReplaysOnly,
            icon: None,
        });
    settings
}

fn marker(settings: &GameSettings) -> usize {
    settings.custom_games[0]
        .name
        .strip_prefix("Marker ")
        .unwrap()
        .parse()
        .unwrap()
}

struct BlockingProbe {
    calls: Mutex<Vec<usize>>,
    entered: Condvar,
    release_first: Mutex<bool>,
    stop_waiting_when_stale: bool,
}

impl BlockingProbe {
    fn new(block_first: bool) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            entered: Condvar::new(),
            release_first: Mutex::new(!block_first),
            stop_waiting_when_stale: true,
        }
    }

    fn uninterruptible_first() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            entered: Condvar::new(),
            release_first: Mutex::new(false),
            stop_waiting_when_stale: false,
        }
    }

    fn wait_for_calls(&self, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut calls = self.calls.lock().unwrap();
        while calls.len() < count {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "detector probe did not run");
            calls = self.entered.wait_timeout(calls, remaining).unwrap().0;
        }
    }

    fn release(&self) {
        *self.release_first.lock().unwrap() = true;
        self.entered.notify_all();
    }
}

impl GameDetectionProbe for BlockingProbe {
    fn detect(
        &self,
        settings: &GameSettings,
        checkpoint: &GameDetectorCheckpoint,
    ) -> Result<Option<DetectedGame>, String> {
        let marker = marker(settings);
        {
            let mut calls = self.calls.lock().unwrap();
            calls.push(marker);
            self.entered.notify_all();
        }
        if self.calls.lock().unwrap().len() == 1 {
            let mut release = self.release_first.lock().unwrap();
            while !*release && (!self.stop_waiting_when_stale || checkpoint.is_current()) {
                release = self
                    .entered
                    .wait_timeout(release, Duration::from_millis(5))
                    .unwrap()
                    .0;
            }
        }
        checkpoint.check().map_err(|error| error.to_string())?;
        Ok(None)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SinkMode {
    Accept,
    Full,
    Disconnected,
}

struct RecordingSink {
    mode: Mutex<SinkMode>,
    attempts: AtomicUsize,
    accepted: Mutex<Vec<GameDetectorToken>>,
    changed: Condvar,
}

impl RecordingSink {
    fn new(mode: SinkMode) -> Self {
        Self {
            mode: Mutex::new(mode),
            attempts: AtomicUsize::new(0),
            accepted: Mutex::new(Vec::new()),
            changed: Condvar::new(),
        }
    }

    fn set_mode(&self, mode: SinkMode) {
        *self.mode.lock().unwrap() = mode;
    }

    fn wait_for_accepted(&self, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut accepted = self.accepted.lock().unwrap();
        while accepted.len() < count {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "detector result was not accepted");
            accepted = self.changed.wait_timeout(accepted, remaining).unwrap().0;
        }
    }
}

impl GameDetectionSink for RecordingSink {
    fn try_publish(
        &self,
        event: GameDetectionEvent,
    ) -> Result<(), Box<RejectedGameDetectionEvent>> {
        self.attempts.fetch_add(1, Ordering::AcqRel);
        match *self.mode.lock().unwrap() {
            SinkMode::Accept => {
                self.accepted.lock().unwrap().push(event.token());
                self.changed.notify_all();
                Ok(())
            }
            SinkMode::Full => Err(Box::new(RejectedGameDetectionEvent {
                error: GameDetectorSinkError::Full,
                event,
            })),
            SinkMode::Disconnected => Err(Box::new(RejectedGameDetectionEvent {
                error: GameDetectorSinkError::Disconnected,
                event,
            })),
        }
    }
}

#[test]
fn staged_reconfiguration_is_inactive_until_commit_and_drop_preserves_current_generation() {
    let probe = Arc::new(BlockingProbe::new(false));
    let sink = Arc::new(RecordingSink::new(SinkMode::Accept));
    let service =
        GameDetectorService::start(config(1), Duration::from_secs(60), probe.clone(), sink)
            .unwrap();
    probe.wait_for_calls(1);
    let initial = service.active_generation();

    let canceled = service.prepare_reconfiguration(config(2)).unwrap();
    assert!(canceled.generation() > initial);
    assert_eq!(service.active_generation(), initial);
    drop(canceled);
    assert_eq!(service.active_generation(), initial);

    service.shutdown().unwrap();
}

#[test]
fn scan_race_and_ten_thousand_save_storm_publish_only_the_latest_generation() {
    let probe = Arc::new(BlockingProbe::uninterruptible_first());
    let sink = Arc::new(RecordingSink::new(SinkMode::Accept));
    let service = GameDetectorService::start(
        config(1),
        Duration::from_secs(60),
        probe.clone(),
        sink.clone(),
    )
    .unwrap();
    probe.wait_for_calls(1);

    let mut latest = service.active_generation();
    for marker in 2..=10_001 {
        let prepared = service.prepare_reconfiguration(config(marker)).unwrap();
        latest = prepared.generation();
        let _ = prepared.commit();
    }
    probe.release();
    probe.wait_for_calls(2);
    sink.wait_for_accepted(1);

    assert_eq!(sink.accepted.lock().unwrap()[0].generation, latest);
    assert_eq!(probe.calls.lock().unwrap().last(), Some(&10_001));
    service.shutdown().unwrap();
}

#[test]
fn full_sink_retains_one_exact_result_and_disconnected_sink_does_not_spin() {
    let probe = Arc::new(BlockingProbe::new(false));
    let sink = Arc::new(RecordingSink::new(SinkMode::Full));
    let service =
        GameDetectorService::start(config(1), Duration::from_millis(10), probe, sink.clone())
            .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while sink.attempts.load(Ordering::Acquire) == 0 {
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    sink.set_mode(SinkMode::Accept);
    sink.wait_for_accepted(1);
    assert_eq!(sink.accepted.lock().unwrap().len(), 1);
    service.shutdown().unwrap();

    let probe = Arc::new(BlockingProbe::new(false));
    let sink = Arc::new(RecordingSink::new(SinkMode::Disconnected));
    let service =
        GameDetectorService::start(config(2), Duration::from_millis(1), probe, sink.clone())
            .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while sink.attempts.load(Ordering::Acquire) == 0 {
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    std::thread::sleep(Duration::from_millis(30));
    assert_eq!(sink.attempts.load(Ordering::Acquire), 1);
    service.shutdown().unwrap();
}

#[test]
fn shutdown_cancels_a_blocked_probe_and_joins_the_only_worker() {
    let probe = Arc::new(BlockingProbe::new(true));
    let sink = Arc::new(RecordingSink::new(SinkMode::Accept));
    let service =
        GameDetectorService::start(config(1), Duration::from_secs(60), probe.clone(), sink)
            .unwrap();
    probe.wait_for_calls(1);
    service.shutdown().unwrap();
    assert_eq!(service.worker_count(), 0);
}

struct FailingSpawner;

impl GameDetectorThreadSpawner for FailingSpawner {
    fn spawn(
        &self,
        _name: &str,
        _task: Box<dyn FnOnce() + Send>,
    ) -> Result<std::thread::JoinHandle<()>, String> {
        Err("injected spawn failure".into())
    }
}

#[test]
fn spawn_failure_returns_without_leaking_a_worker() {
    let error = match GameDetectorService::start_with_spawner(
        config(1),
        Duration::from_secs(1),
        Arc::new(BlockingProbe::new(false)),
        Arc::new(RecordingSink::new(SinkMode::Accept)),
        &FailingSpawner,
    ) {
        Ok(_) => panic!("injected spawn failure must reject service construction"),
        Err(error) => error,
    };
    assert_eq!(
        error,
        GameDetectorServiceError::Spawn("injected spawn failure".into())
    );
}

struct PanickingProbe;

impl GameDetectionProbe for PanickingProbe {
    fn detect(
        &self,
        _settings: &GameSettings,
        _checkpoint: &GameDetectorCheckpoint,
    ) -> Result<Option<DetectedGame>, String> {
        panic!("injected detector panic")
    }
}

#[test]
fn repeated_probe_panic_is_contained_and_published_only_once() {
    let sink = Arc::new(RecordingSink::new(SinkMode::Accept));
    let service = GameDetectorService::start(
        config(1),
        Duration::from_millis(1),
        Arc::new(PanickingProbe),
        sink.clone(),
    )
    .unwrap();
    sink.wait_for_accepted(1);
    std::thread::sleep(Duration::from_millis(30));
    assert_eq!(sink.attempts.load(Ordering::Acquire), 1);
    service.shutdown().unwrap();
}

#[test]
fn identical_failure_is_published_once_for_each_exact_configuration_generation() {
    let sink = Arc::new(RecordingSink::new(SinkMode::Accept));
    let service = GameDetectorService::start(
        config(1),
        Duration::from_millis(1),
        Arc::new(PanickingProbe),
        sink.clone(),
    )
    .unwrap();
    sink.wait_for_accepted(1);
    let first = service.active_generation();
    let prepared = service.prepare_reconfiguration(config(2)).unwrap();
    let second = prepared.generation();
    let _ = prepared.commit();
    sink.wait_for_accepted(2);
    assert_eq!(
        sink.accepted
            .lock()
            .unwrap()
            .iter()
            .map(|token| token.generation)
            .collect::<Vec<_>>(),
        [first, second]
    );
    service.shutdown().unwrap();
}

#[test]
fn invalid_reconfiguration_fails_before_advancing_or_replacing_authority() {
    let service = GameDetectorService::start(
        config(1),
        Duration::from_secs(60),
        Arc::new(BlockingProbe::new(false)),
        Arc::new(RecordingSink::new(SinkMode::Accept)),
    )
    .unwrap();
    let before = service.active_generation();
    let mut invalid = GameSettings::default();
    for index in 0..=clipline_settings::MAX_SETTINGS_CUSTOM_GAMES {
        invalid
            .custom_games
            .push(config(index).custom_games.remove(0));
    }
    let error = match service.prepare_reconfiguration(invalid) {
        Ok(_) => panic!("oversized detector config must be rejected"),
        Err(error) => error,
    };
    assert!(matches!(error, GameDetectorServiceError::InvalidConfig(_)));
    assert_eq!(service.active_generation(), before);
    service.shutdown().unwrap();
}

struct BlockingSink {
    entered: Mutex<bool>,
    release: Mutex<bool>,
    changed: Condvar,
}

impl BlockingSink {
    fn new() -> Self {
        Self {
            entered: Mutex::new(false),
            release: Mutex::new(false),
            changed: Condvar::new(),
        }
    }

    fn wait_until_entered(&self) {
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut entered = self.entered.lock().unwrap();
        while !*entered {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "detector sink was not entered");
            entered = self.changed.wait_timeout(entered, remaining).unwrap().0;
        }
    }
}

impl GameDetectionSink for BlockingSink {
    fn try_publish(
        &self,
        _event: GameDetectionEvent,
    ) -> Result<(), Box<RejectedGameDetectionEvent>> {
        *self.entered.lock().unwrap() = true;
        self.changed.notify_all();
        let mut release = self.release.lock().unwrap();
        while !*release {
            release = self.changed.wait(release).unwrap();
        }
        Ok(())
    }
}

#[test]
fn configuration_commit_linearizes_after_inflight_recorder_and_event_intent() {
    let sink = Arc::new(BlockingSink::new());
    let service = GameDetectorService::start(
        config(1),
        Duration::from_secs(60),
        Arc::new(BlockingProbe::new(false)),
        sink.clone(),
    )
    .unwrap();
    sink.wait_until_entered();
    let initial = service.active_generation();
    let prepared = service.prepare_reconfiguration(config(2)).unwrap();
    let replacement = prepared.generation();
    let commit = std::thread::spawn(move || prepared.commit());
    std::thread::sleep(Duration::from_millis(20));
    assert_eq!(service.active_generation(), initial);
    *sink.release.lock().unwrap() = true;
    sink.changed.notify_all();
    let _ = commit.join().unwrap();
    assert_eq!(service.active_generation(), replacement);
    service.shutdown().unwrap();
}

struct FailingIntentSink {
    notices: AtomicUsize,
    changed: Condvar,
    wake: Mutex<()>,
}

impl GameDetectionSink for FailingIntentSink {
    fn try_publish(
        &self,
        event: GameDetectionEvent,
    ) -> Result<(), Box<RejectedGameDetectionEvent>> {
        match event {
            event @ GameDetectionEvent::Detection { .. } => {
                Err(Box::new(RejectedGameDetectionEvent {
                    error: GameDetectorSinkError::Failed("recorder restart failed".into()),
                    event,
                }))
            }
            GameDetectionEvent::Failed { .. } => {
                self.notices.fetch_add(1, Ordering::AcqRel);
                self.changed.notify_all();
                Ok(())
            }
        }
    }
}

#[test]
fn repeated_recorder_intent_failure_publishes_one_bounded_notice_per_generation() {
    let sink = Arc::new(FailingIntentSink {
        notices: AtomicUsize::new(0),
        changed: Condvar::new(),
        wake: Mutex::new(()),
    });
    let service = GameDetectorService::start(
        config(1),
        Duration::from_millis(1),
        Arc::new(BlockingProbe::new(false)),
        sink.clone(),
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut wake = sink.wake.lock().unwrap();
    while sink.notices.load(Ordering::Acquire) == 0 {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "detector failure notice was not published"
        );
        wake = sink.changed.wait_timeout(wake, remaining).unwrap().0;
    }
    drop(wake);
    std::thread::sleep(Duration::from_millis(30));
    assert_eq!(sink.notices.load(Ordering::Acquire), 1);
    service.shutdown().unwrap();
}

#[test]
fn reversible_quiescence_stops_publication_and_resume_uses_the_latest_config() {
    let sink = Arc::new(RecordingSink::new(SinkMode::Accept));
    let service = GameDetectorService::start(
        config(1),
        Duration::from_millis(1),
        Arc::new(BlockingProbe::new(false)),
        sink.clone(),
    )
    .unwrap();
    sink.wait_for_accepted(1);
    service.quiesce().unwrap();
    let attempts = sink.attempts.load(Ordering::Acquire);
    let prepared = service.prepare_reconfiguration(config(2)).unwrap();
    let latest = prepared.generation();
    let _ = prepared.commit();
    std::thread::sleep(Duration::from_millis(20));
    assert_eq!(sink.attempts.load(Ordering::Acquire), attempts);
    service.resume().unwrap();
    sink.wait_for_accepted(2);
    assert_eq!(
        sink.accepted.lock().unwrap().last().unwrap().generation,
        latest
    );
    service.shutdown().unwrap();
}

#[test]
fn quiesce_resume_invalidates_a_probe_that_was_already_in_flight() {
    let probe = Arc::new(BlockingProbe::uninterruptible_first());
    let sink = Arc::new(RecordingSink::new(SinkMode::Accept));
    let service = GameDetectorService::start(
        config(1),
        Duration::from_millis(1),
        probe.clone(),
        sink.clone(),
    )
    .unwrap();
    probe.wait_for_calls(1);
    let before = service.active_token();

    service.quiesce().unwrap();
    service.resume().unwrap();
    probe.release();
    probe.wait_for_calls(2);
    sink.wait_for_accepted(1);

    let accepted = sink.accepted.lock().unwrap();
    assert_eq!(accepted.len(), 1);
    assert_eq!(accepted[0].generation, before.generation);
    assert!(accepted[0].work_epoch > before.work_epoch);
    drop(accepted);
    service.shutdown().unwrap();
}

#[test]
fn quiesce_resume_discards_a_full_result_and_runs_a_fresh_probe() {
    let probe = Arc::new(BlockingProbe::new(false));
    let sink = Arc::new(RecordingSink::new(SinkMode::Full));
    let service = GameDetectorService::start(
        config(1),
        Duration::from_millis(1),
        probe.clone(),
        sink.clone(),
    )
    .unwrap();
    probe.wait_for_calls(1);
    let deadline = Instant::now() + Duration::from_secs(2);
    while sink.attempts.load(Ordering::Acquire) == 0 {
        assert!(
            Instant::now() < deadline,
            "detector result was not attempted"
        );
        std::thread::yield_now();
    }
    let before = service.active_token();

    service.quiesce().unwrap();
    service.resume().unwrap();
    sink.set_mode(SinkMode::Accept);
    probe.wait_for_calls(2);
    sink.wait_for_accepted(1);

    let accepted = sink.accepted.lock().unwrap();
    assert_eq!(accepted.len(), 1);
    assert!(accepted[0].work_epoch > before.work_epoch);
    drop(accepted);
    service.shutdown().unwrap();
}
