use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use clipline_recorder::microphone::{
    MicrophoneMonitorConfig, MicrophoneMonitorEvent, MicrophoneMonitorEventSink,
    MicrophoneMonitorFactory, MicrophoneMonitorRenderer, MicrophoneMonitorService,
    MicrophoneMonitorSource, MicrophoneOutputMode, MicrophoneStopToken,
    MAX_MICROPHONE_MONITOR_SAMPLES,
};

#[derive(Default)]
struct Events {
    events: Mutex<Vec<MicrophoneMonitorEvent>>,
    changed: Condvar,
}

impl Events {
    fn wait_for(&self, predicate: impl Fn(&[MicrophoneMonitorEvent]) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut events = self.events.lock().unwrap();
        while !predicate(&events) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "timed out waiting for events: {events:?}"
            );
            (events, _) = self.changed.wait_timeout(events, remaining).unwrap();
        }
    }

    fn snapshot(&self) -> Vec<MicrophoneMonitorEvent> {
        self.events.lock().unwrap().clone()
    }
}

impl MicrophoneMonitorEventSink for Events {
    fn try_publish(&self, event: MicrophoneMonitorEvent) -> Result<(), String> {
        self.events.lock().unwrap().push(event);
        self.changed.notify_all();
        Ok(())
    }
}

struct ScriptedSource {
    chunks: VecDeque<Result<Vec<f32>, String>>,
    fallback: Vec<f32>,
    polls: Arc<AtomicUsize>,
}

impl MicrophoneMonitorSource for ScriptedSource {
    fn poll_48khz_stereo(&mut self, samples: &mut Vec<f32>) -> Result<(), String> {
        self.polls.fetch_add(1, Ordering::AcqRel);
        match self.chunks.pop_front() {
            Some(Ok(chunk)) => samples.extend(chunk),
            Some(Err(error)) => return Err(error),
            None => samples.extend_from_slice(&self.fallback),
        }
        Ok(())
    }
}

struct CountingRenderer {
    writes: Arc<AtomicUsize>,
    accepted_frames: Option<usize>,
}

impl MicrophoneMonitorRenderer for CountingRenderer {
    fn write_48khz_stereo(&mut self, samples: &[f32]) -> Result<usize, String> {
        self.writes.fetch_add(1, Ordering::AcqRel);
        Ok(self
            .accepted_frames
            .unwrap_or(samples.len() / 2)
            .min(samples.len() / 2))
    }
}

type SourceScript = VecDeque<Result<Vec<f32>, String>>;

struct FakeFactory {
    chunks: Mutex<VecDeque<SourceScript>>,
    opened_sources: AtomicUsize,
    opened_renderers: AtomicUsize,
    dropped_sources: Arc<AtomicUsize>,
    polls: Arc<AtomicUsize>,
    writes: Arc<AtomicUsize>,
}

impl FakeFactory {
    fn new(sessions: impl IntoIterator<Item = SourceScript>) -> Self {
        Self {
            chunks: Mutex::new(sessions.into_iter().collect()),
            opened_sources: AtomicUsize::new(0),
            opened_renderers: AtomicUsize::new(0),
            dropped_sources: Arc::new(AtomicUsize::new(0)),
            polls: Arc::new(AtomicUsize::new(0)),
            writes: Arc::new(AtomicUsize::new(0)),
        }
    }
}

struct DropSource {
    inner: ScriptedSource,
    dropped: Arc<AtomicUsize>,
}

impl MicrophoneMonitorSource for DropSource {
    fn poll_48khz_stereo(&mut self, samples: &mut Vec<f32>) -> Result<(), String> {
        self.inner.poll_48khz_stereo(samples)
    }
}

impl Drop for DropSource {
    fn drop(&mut self) {
        self.dropped.fetch_add(1, Ordering::AcqRel);
    }
}

impl MicrophoneMonitorFactory for FakeFactory {
    fn open_source(
        &self,
        _config: &MicrophoneMonitorConfig,
        _stop: &MicrophoneStopToken,
    ) -> Result<Box<dyn MicrophoneMonitorSource>, String> {
        self.opened_sources.fetch_add(1, Ordering::AcqRel);
        let chunks = self.chunks.lock().unwrap().pop_front().unwrap_or_default();
        Ok(Box::new(DropSource {
            inner: ScriptedSource {
                chunks,
                fallback: vec![0.25, -0.25],
                polls: Arc::clone(&self.polls),
            },
            dropped: Arc::clone(&self.dropped_sources),
        }))
    }

    fn open_renderer(
        &self,
        _generation: u64,
        _stop: &MicrophoneStopToken,
    ) -> Result<Box<dyn MicrophoneMonitorRenderer>, String> {
        self.opened_renderers.fetch_add(1, Ordering::AcqRel);
        Ok(Box::new(CountingRenderer {
            writes: Arc::clone(&self.writes),
            accepted_frames: None,
        }))
    }
}

fn config(output: MicrophoneOutputMode) -> MicrophoneMonitorConfig {
    MicrophoneMonitorConfig {
        device_id: None,
        volume: 1.0,
        mono: false,
        output,
    }
}

#[test]
fn tauri_mode_publishes_exact_bounded_pcm_without_opening_a_renderer() {
    let samples = vec![0.5, -0.5, 1.0, -1.0];
    let factory = Arc::new(FakeFactory::new([VecDeque::from([Ok(samples)])]));
    let events = Arc::new(Events::default());
    let service = MicrophoneMonitorService::new(factory.clone(), events.clone());

    let generation = service
        .start(config(MicrophoneOutputMode::TauriCompatibilityPcm))
        .unwrap();
    events.wait_for(|events| {
        events.iter().any(|event| {
            matches!(event, MicrophoneMonitorEvent::Monitor { generation: event_generation, sample_count: 4, pcm_i16: Some(samples), .. } if *event_generation == generation && samples.len() == 4)
        })
    });
    service.stop().unwrap();

    assert_eq!(factory.opened_renderers.load(Ordering::Acquire), 0);
    assert_eq!(factory.dropped_sources.load(Ordering::Acquire), 1);
    let events = events.snapshot();
    let pcm = events.iter().find_map(|event| match event {
        MicrophoneMonitorEvent::Monitor {
            generation: active,
            pcm_i16: Some(samples),
            ..
        } if *active == generation => Some(samples.as_slice()),
        _ => None,
    });
    assert_eq!(pcm, Some([16_384, -16_384, 32_767, -32_768].as_slice()));
    assert!(
        matches!(events.last(), Some(MicrophoneMonitorEvent::Stopped { generation: stopped }) if *stopped == generation)
    );
}

#[test]
fn native_mode_renders_without_publishing_pcm_or_growing_a_queue() {
    let factory = Arc::new(FakeFactory::new([VecDeque::from([Ok(vec![0.25; 8])])]));
    let events = Arc::new(Events::default());
    let service = MicrophoneMonitorService::new(factory.clone(), events.clone());
    let generation = service
        .start(config(MicrophoneOutputMode::NativeRenderer))
        .unwrap();
    events.wait_for(|events| {
        events.iter().any(|event| {
            matches!(event, MicrophoneMonitorEvent::Monitor { generation: event_generation, sample_count: 8, pcm_i16: None, .. } if *event_generation == generation)
        })
    });
    service.stop().unwrap();

    assert_eq!(factory.opened_renderers.load(Ordering::Acquire), 1);
    assert!(factory.writes.load(Ordering::Acquire) >= 1);
}

#[test]
fn replacement_joins_and_releases_the_old_generation_before_new_output() {
    let factory = Arc::new(FakeFactory::new([
        VecDeque::from([Ok(vec![0.1; 4])]),
        VecDeque::from([Ok(vec![0.2; 4])]),
    ]));
    let events = Arc::new(Events::default());
    let service = MicrophoneMonitorService::new(factory.clone(), events.clone());
    let first = service
        .start(config(MicrophoneOutputMode::TauriCompatibilityPcm))
        .unwrap();
    events.wait_for(|events| events.iter().any(|event| matches!(event, MicrophoneMonitorEvent::Monitor { generation, .. } if *generation == first)));
    let second = service
        .start(config(MicrophoneOutputMode::TauriCompatibilityPcm))
        .unwrap();
    events.wait_for(|events| events.iter().any(|event| matches!(event, MicrophoneMonitorEvent::Monitor { generation, .. } if *generation == second)));
    service.stop().unwrap();

    let events = events.snapshot();
    let first_stopped = events.iter().position(|event| matches!(event, MicrophoneMonitorEvent::Stopped { generation } if *generation == first)).unwrap();
    let second_monitor = events.iter().position(|event| matches!(event, MicrophoneMonitorEvent::Monitor { generation, .. } if *generation == second)).unwrap();
    assert!(first_stopped < second_monitor);
    assert_eq!(factory.dropped_sources.load(Ordering::Acquire), 2);
}

#[test]
fn oversized_nonfinite_and_source_errors_fail_with_error_then_stopped() {
    for bad in [
        Ok(vec![0.0; MAX_MICROPHONE_MONITOR_SAMPLES + 2]),
        Ok(vec![f32::NAN, 0.0]),
        Err("device failed".into()),
    ] {
        let factory = Arc::new(FakeFactory::new([VecDeque::from([bad])]));
        let events = Arc::new(Events::default());
        let service = MicrophoneMonitorService::new(factory, events.clone());
        let generation = service
            .start(config(MicrophoneOutputMode::TauriCompatibilityPcm))
            .unwrap();
        events.wait_for(|events| events.iter().any(|event| matches!(event, MicrophoneMonitorEvent::Stopped { generation: stopped } if *stopped == generation)));
        let events = events.snapshot();
        let error = events.iter().position(|event| matches!(event, MicrophoneMonitorEvent::Error { generation: failed, .. } if *failed == generation)).unwrap();
        let stopped = events.iter().position(|event| matches!(event, MicrophoneMonitorEvent::Stopped { generation: ended } if *ended == generation)).unwrap();
        assert!(error < stopped);
    }
}

#[test]
fn one_hundred_start_stop_cycles_join_every_worker_and_source() {
    let factory = Arc::new(FakeFactory::new(
        std::iter::repeat_with(VecDeque::new).take(100),
    ));
    let events = Arc::new(Events::default());
    let service = MicrophoneMonitorService::new(factory.clone(), events);
    for _ in 0..100 {
        service
            .start(config(MicrophoneOutputMode::TauriCompatibilityPcm))
            .unwrap();
        service.stop().unwrap();
        assert_eq!(service.worker_count(), 0);
    }
    assert_eq!(factory.dropped_sources.load(Ordering::Acquire), 100);
    assert!(service.active_generation().is_none());
}

#[test]
fn shutdown_joins_current_work_and_permanently_rejects_restart() {
    let factory = Arc::new(FakeFactory::new([VecDeque::new()]));
    let events = Arc::new(Events::default());
    let service = MicrophoneMonitorService::new(factory.clone(), events.clone());
    let generation = service
        .start(config(MicrophoneOutputMode::TauriCompatibilityPcm))
        .unwrap();
    events.wait_for(|events| {
        events.iter().any(|event| {
            matches!(event, MicrophoneMonitorEvent::Monitor { generation: active, .. } if *active == generation)
        })
    });
    service.shutdown().unwrap();

    assert!(service
        .start(config(MicrophoneOutputMode::TauriCompatibilityPcm))
        .is_err());
    assert_eq!(service.worker_count(), 0);
    assert_eq!(factory.dropped_sources.load(Ordering::Acquire), 1);
}

#[derive(Default)]
struct PhaseLatch {
    reached: Mutex<bool>,
    changed: Condvar,
}

impl PhaseLatch {
    fn reach(&self) {
        *self.reached.lock().unwrap() = true;
        self.changed.notify_all();
    }

    fn wait(&self) {
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut reached = self.reached.lock().unwrap();
        while !*reached {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "timed out waiting for worker phase");
            (reached, _) = self.changed.wait_timeout(reached, remaining).unwrap();
        }
    }
}

struct BlockingActivationFactory {
    entered: Arc<PhaseLatch>,
}

impl MicrophoneMonitorFactory for BlockingActivationFactory {
    fn open_source(
        &self,
        _config: &MicrophoneMonitorConfig,
        stop: &MicrophoneStopToken,
    ) -> Result<Box<dyn MicrophoneMonitorSource>, String> {
        self.entered.reach();
        while !stop.is_stopped() {
            std::thread::yield_now();
        }
        Err("activation canceled".into())
    }

    fn open_renderer(
        &self,
        _generation: u64,
        _stop: &MicrophoneStopToken,
    ) -> Result<Box<dyn MicrophoneMonitorRenderer>, String> {
        unreachable!("Tauri mode must not activate a renderer")
    }
}

struct BlockingPollSource {
    entered: Arc<PhaseLatch>,
    stop: MicrophoneStopToken,
}

impl MicrophoneMonitorSource for BlockingPollSource {
    fn poll_48khz_stereo(&mut self, _samples: &mut Vec<f32>) -> Result<(), String> {
        self.entered.reach();
        while !self.stop.is_stopped() {
            std::thread::yield_now();
        }
        Ok(())
    }
}

struct BlockingPollFactory {
    entered: Arc<PhaseLatch>,
}

impl MicrophoneMonitorFactory for BlockingPollFactory {
    fn open_source(
        &self,
        _config: &MicrophoneMonitorConfig,
        stop: &MicrophoneStopToken,
    ) -> Result<Box<dyn MicrophoneMonitorSource>, String> {
        Ok(Box::new(BlockingPollSource {
            entered: Arc::clone(&self.entered),
            stop: stop.clone(),
        }))
    }

    fn open_renderer(
        &self,
        _generation: u64,
        _stop: &MicrophoneStopToken,
    ) -> Result<Box<dyn MicrophoneMonitorRenderer>, String> {
        unreachable!("Tauri mode must not activate a renderer")
    }
}

struct FixedSource;

impl MicrophoneMonitorSource for FixedSource {
    fn poll_48khz_stereo(&mut self, samples: &mut Vec<f32>) -> Result<(), String> {
        samples.extend_from_slice(&[0.25, -0.25]);
        Ok(())
    }
}

struct BlockingRenderer {
    entered: Arc<PhaseLatch>,
    stop: MicrophoneStopToken,
}

impl MicrophoneMonitorRenderer for BlockingRenderer {
    fn write_48khz_stereo(&mut self, _samples: &[f32]) -> Result<usize, String> {
        self.entered.reach();
        while !self.stop.is_stopped() {
            std::thread::yield_now();
        }
        Ok(0)
    }
}

struct BlockingRendererFactory {
    entered: Arc<PhaseLatch>,
}

impl MicrophoneMonitorFactory for BlockingRendererFactory {
    fn open_source(
        &self,
        _config: &MicrophoneMonitorConfig,
        _stop: &MicrophoneStopToken,
    ) -> Result<Box<dyn MicrophoneMonitorSource>, String> {
        Ok(Box::new(FixedSource))
    }

    fn open_renderer(
        &self,
        _generation: u64,
        stop: &MicrophoneStopToken,
    ) -> Result<Box<dyn MicrophoneMonitorRenderer>, String> {
        Ok(Box::new(BlockingRenderer {
            entered: Arc::clone(&self.entered),
            stop: stop.clone(),
        }))
    }
}

fn assert_joined_stop_during_phase(
    factory: Arc<dyn MicrophoneMonitorFactory>,
    entered: &PhaseLatch,
    output: MicrophoneOutputMode,
) {
    let events = Arc::new(Events::default());
    let service = MicrophoneMonitorService::new(factory, events.clone());
    let generation = service.start(config(output)).unwrap();
    entered.wait();
    service.stop().unwrap();
    assert_eq!(service.worker_count(), 0);
    let snapshot = events.snapshot();
    assert!(
        matches!(snapshot.last(), Some(MicrophoneMonitorEvent::Stopped { generation: stopped }) if *stopped == generation)
    );
    assert!(!snapshot.iter().any(|event| matches!(
        event,
        MicrophoneMonitorEvent::Monitor { generation: active, .. } if *active == generation
    )));
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(events.snapshot(), snapshot, "joined stop is an event fence");
}

#[test]
fn stop_joins_activation_poll_and_renderer_write_without_late_samples() {
    let activation = Arc::new(PhaseLatch::default());
    assert_joined_stop_during_phase(
        Arc::new(BlockingActivationFactory {
            entered: Arc::clone(&activation),
        }),
        &activation,
        MicrophoneOutputMode::TauriCompatibilityPcm,
    );

    let poll = Arc::new(PhaseLatch::default());
    assert_joined_stop_during_phase(
        Arc::new(BlockingPollFactory {
            entered: Arc::clone(&poll),
        }),
        &poll,
        MicrophoneOutputMode::TauriCompatibilityPcm,
    );

    let render = Arc::new(PhaseLatch::default());
    assert_joined_stop_during_phase(
        Arc::new(BlockingRendererFactory {
            entered: Arc::clone(&render),
        }),
        &render,
        MicrophoneOutputMode::NativeRenderer,
    );
}

struct PanickingSource;

impl MicrophoneMonitorSource for PanickingSource {
    fn poll_48khz_stereo(&mut self, _samples: &mut Vec<f32>) -> Result<(), String> {
        panic!("injected microphone source panic");
    }
}

struct PanickingFactory;

impl MicrophoneMonitorFactory for PanickingFactory {
    fn open_source(
        &self,
        _config: &MicrophoneMonitorConfig,
        _stop: &MicrophoneStopToken,
    ) -> Result<Box<dyn MicrophoneMonitorSource>, String> {
        Ok(Box::new(PanickingSource))
    }

    fn open_renderer(
        &self,
        _generation: u64,
        _stop: &MicrophoneStopToken,
    ) -> Result<Box<dyn MicrophoneMonitorRenderer>, String> {
        unreachable!("Tauri mode must not activate a renderer")
    }
}

#[test]
fn worker_panic_is_bounded_to_error_then_stopped_and_remains_joinable() {
    let events = Arc::new(Events::default());
    let service = MicrophoneMonitorService::new(Arc::new(PanickingFactory), events.clone());
    let generation = service
        .start(config(MicrophoneOutputMode::TauriCompatibilityPcm))
        .unwrap();
    events.wait_for(|events| {
        events.iter().any(
            |event| matches!(event, MicrophoneMonitorEvent::Stopped { generation: stopped } if *stopped == generation),
        )
    });
    service.stop().unwrap();
    let events = events.snapshot();
    assert!(matches!(
        events.as_slice(),
        [MicrophoneMonitorEvent::Error { generation: failed, message }, MicrophoneMonitorEvent::Stopped { generation: stopped }]
            if *failed == generation && *stopped == generation && message == "microphone monitor worker panicked"
    ));
    assert_eq!(service.worker_count(), 0);
}

struct ReplacementDuringActivationFactory {
    opened: AtomicUsize,
    first_entered: Arc<PhaseLatch>,
}

impl MicrophoneMonitorFactory for ReplacementDuringActivationFactory {
    fn open_source(
        &self,
        _config: &MicrophoneMonitorConfig,
        stop: &MicrophoneStopToken,
    ) -> Result<Box<dyn MicrophoneMonitorSource>, String> {
        if self.opened.fetch_add(1, Ordering::AcqRel) == 0 {
            self.first_entered.reach();
            while !stop.is_stopped() {
                std::thread::yield_now();
            }
            return Err("first activation superseded".into());
        }
        Ok(Box::new(FixedSource))
    }

    fn open_renderer(
        &self,
        _generation: u64,
        _stop: &MicrophoneStopToken,
    ) -> Result<Box<dyn MicrophoneMonitorRenderer>, String> {
        unreachable!("Tauri mode must not activate a renderer")
    }
}

#[test]
fn replacement_during_activation_joins_the_old_generation_before_new_output() {
    let entered = Arc::new(PhaseLatch::default());
    let factory = Arc::new(ReplacementDuringActivationFactory {
        opened: AtomicUsize::new(0),
        first_entered: Arc::clone(&entered),
    });
    let events = Arc::new(Events::default());
    let service = MicrophoneMonitorService::new(factory.clone(), events.clone());
    let first = service
        .start(config(MicrophoneOutputMode::TauriCompatibilityPcm))
        .unwrap();
    entered.wait();
    let second = service
        .start(config(MicrophoneOutputMode::TauriCompatibilityPcm))
        .unwrap();
    events.wait_for(|events| {
        events.iter().any(
            |event| matches!(event, MicrophoneMonitorEvent::Monitor { generation, .. } if *generation == second),
        )
    });
    service.stop().unwrap();

    let events = events.snapshot();
    let first_stopped = events
        .iter()
        .position(
            |event| matches!(event, MicrophoneMonitorEvent::Stopped { generation } if *generation == first),
        )
        .unwrap();
    let second_monitor = events
        .iter()
        .position(
            |event| matches!(event, MicrophoneMonitorEvent::Monitor { generation, .. } if *generation == second),
        )
        .unwrap();
    assert!(first_stopped < second_monitor);
    assert_eq!(factory.opened.load(Ordering::Acquire), 2);
}

struct ZeroBackpressureRenderer(Arc<AtomicUsize>);

impl MicrophoneMonitorRenderer for ZeroBackpressureRenderer {
    fn write_48khz_stereo(&mut self, _samples: &[f32]) -> Result<usize, String> {
        self.0.fetch_add(1, Ordering::AcqRel);
        Ok(0)
    }
}

struct ZeroBackpressureFactory(Arc<AtomicUsize>);

impl MicrophoneMonitorFactory for ZeroBackpressureFactory {
    fn open_source(
        &self,
        _config: &MicrophoneMonitorConfig,
        _stop: &MicrophoneStopToken,
    ) -> Result<Box<dyn MicrophoneMonitorSource>, String> {
        Ok(Box::new(FixedSource))
    }

    fn open_renderer(
        &self,
        _generation: u64,
        _stop: &MicrophoneStopToken,
    ) -> Result<Box<dyn MicrophoneMonitorRenderer>, String> {
        Ok(Box::new(ZeroBackpressureRenderer(Arc::clone(&self.0))))
    }
}

#[test]
fn zero_renderer_backpressure_drops_remainder_without_queueing_or_stalling() {
    let writes = Arc::new(AtomicUsize::new(0));
    let events = Arc::new(Events::default());
    let service = MicrophoneMonitorService::new(
        Arc::new(ZeroBackpressureFactory(Arc::clone(&writes))),
        events.clone(),
    );
    let generation = service
        .start(config(MicrophoneOutputMode::NativeRenderer))
        .unwrap();
    events.wait_for(|events| {
        events
            .iter()
            .filter(
                |event| matches!(event, MicrophoneMonitorEvent::Monitor { generation: active, .. } if *active == generation),
            )
            .count()
            >= 2
    });
    service.stop().unwrap();
    assert!(writes.load(Ordering::Acquire) >= 2);
    assert_eq!(service.worker_count(), 0);
}
