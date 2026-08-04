//! Joined, frontend-independent microphone monitor lifecycle.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use thiserror::Error;

pub const MICROPHONE_SAMPLE_RATE: u32 = 48_000;
pub const MICROPHONE_CHANNELS: usize = 2;
pub const MAX_MICROPHONE_MONITOR_SAMPLES: usize = 4_096;
pub const MAX_MICROPHONE_DEVICE_ID_BYTES: usize = 16 * 1024;
pub const MAX_MICROPHONE_ERROR_BYTES: usize = 64 * 1024;
const MONITOR_POLL_INTERVAL: Duration = Duration::from_millis(30);
const DEFAULT_RENDER_ENDPOINT_RECHECK_INTERVAL: Duration = Duration::from_secs(1);

static ACTIVE_MICROPHONE_WORKERS: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicrophoneOutputMode {
    TauriCompatibilityPcm,
    NativeRenderer,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MicrophoneMonitorConfig {
    pub device_id: Option<String>,
    pub volume: f32,
    pub mono: bool,
    pub output: MicrophoneOutputMode,
}

impl MicrophoneMonitorConfig {
    pub fn validate(&self) -> Result<(), MicrophoneServiceError> {
        if self
            .device_id
            .as_ref()
            .is_some_and(|device_id| device_id.len() > MAX_MICROPHONE_DEVICE_ID_BYTES)
        {
            return Err(MicrophoneServiceError::InvalidConfig(
                "microphone device id exceeds its byte bound".into(),
            ));
        }
        if !self.volume.is_finite() || !(0.0..=2.0).contains(&self.volume) {
            return Err(MicrophoneServiceError::InvalidConfig(
                "microphone volume must be finite and between zero and two".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum MicrophoneMonitorEvent {
    Monitor {
        generation: u64,
        rms: f32,
        peak: f32,
        sample_count: usize,
        /// Present only for the shipping Tauri/WebView compatibility path.
        pcm_i16: Option<Vec<i16>>,
    },
    Error {
        generation: u64,
        message: String,
    },
    Stopped {
        generation: u64,
    },
}

pub trait MicrophoneMonitorEventSink: Send + Sync + 'static {
    fn try_publish(&self, event: MicrophoneMonitorEvent) -> Result<(), String>;
}

/// A capture source that appends one bounded chunk of interleaved 48 kHz
/// stereo-f32 samples into the caller-owned reusable buffer.
pub trait MicrophoneMonitorSource: 'static {
    fn poll_48khz_stereo(&mut self, samples: &mut Vec<f32>) -> Result<(), String>;
}

/// A real-time renderer. It must not retain or queue the supplied slice and
/// returns the number of stereo frames accepted without blocking.
pub trait MicrophoneMonitorRenderer: 'static {
    fn write_48khz_stereo(&mut self, samples: &[f32]) -> Result<usize, String>;
}

pub trait MicrophoneMonitorFactory: Send + Sync + 'static {
    fn open_source(
        &self,
        config: &MicrophoneMonitorConfig,
        stop: &MicrophoneStopToken,
    ) -> Result<Box<dyn MicrophoneMonitorSource>, String>;

    fn open_renderer(
        &self,
        generation: u64,
        stop: &MicrophoneStopToken,
    ) -> Result<Box<dyn MicrophoneMonitorRenderer>, String>;
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MicrophoneServiceError {
    #[error("invalid microphone monitor configuration: {0}")]
    InvalidConfig(String),
    #[error("microphone monitor generation exhausted")]
    GenerationExhausted,
    #[error("could not start microphone monitor worker: {0}")]
    Spawn(String),
    #[error("microphone monitor worker panicked")]
    WorkerPanicked,
    #[error("microphone monitor lifecycle lock is poisoned")]
    Poisoned,
    #[error("microphone monitor service is shut down")]
    ShutDown,
}

#[derive(Default)]
struct StopState {
    stopped: AtomicBool,
    wake: Mutex<()>,
    changed: Condvar,
}

#[derive(Clone, Default)]
pub struct MicrophoneStopToken(Arc<StopState>);

impl MicrophoneStopToken {
    #[must_use]
    pub fn is_stopped(&self) -> bool {
        self.0.stopped.load(Ordering::Acquire)
    }

    fn stop(&self) {
        self.0.stopped.store(true, Ordering::Release);
        self.0.changed.notify_all();
    }

    fn wait_or_stopped(&self, duration: Duration) -> bool {
        if self.is_stopped() {
            return true;
        }
        let Ok(guard) = self.0.wake.lock() else {
            return true;
        };
        if self.is_stopped() {
            return true;
        }
        let _ = self.0.changed.wait_timeout(guard, duration);
        self.is_stopped()
    }
}

enum StartGate {
    Start,
    Cancel,
}

struct MicrophoneSession {
    generation: u64,
    stop: MicrophoneStopToken,
    gate: mpsc::SyncSender<StartGate>,
    worker: JoinHandle<()>,
}

impl MicrophoneSession {
    fn cancel_and_join(self) -> Result<(), MicrophoneServiceError> {
        self.stop.stop();
        let _ = self.gate.try_send(StartGate::Cancel);
        self.worker
            .join()
            .map_err(|_| MicrophoneServiceError::WorkerPanicked)
    }
}

#[derive(Default)]
struct MicrophoneState {
    last_generation: u64,
    active: Option<MicrophoneSession>,
    shut_down: bool,
}

pub struct MicrophoneMonitorService {
    factory: Arc<dyn MicrophoneMonitorFactory>,
    sink: Arc<dyn MicrophoneMonitorEventSink>,
    operation: Mutex<()>,
    state: Mutex<MicrophoneState>,
    workers: Arc<AtomicUsize>,
}

impl MicrophoneMonitorService {
    #[must_use]
    pub fn new(
        factory: Arc<dyn MicrophoneMonitorFactory>,
        sink: Arc<dyn MicrophoneMonitorEventSink>,
    ) -> Self {
        Self {
            factory,
            sink,
            operation: Mutex::new(()),
            state: Mutex::new(MicrophoneState::default()),
            workers: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn start(&self, config: MicrophoneMonitorConfig) -> Result<u64, MicrophoneServiceError> {
        config.validate()?;
        let _operation = self
            .operation
            .lock()
            .map_err(|_| MicrophoneServiceError::Poisoned)?;
        let generation = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| MicrophoneServiceError::Poisoned)?;
            if state.shut_down {
                return Err(MicrophoneServiceError::ShutDown);
            }
            state.last_generation = state
                .last_generation
                .checked_add(1)
                .ok_or(MicrophoneServiceError::GenerationExhausted)?;
            state.last_generation
        };

        let stop = MicrophoneStopToken::default();
        let (gate, gate_rx) = mpsc::sync_channel(1);
        let worker_stop = stop.clone();
        let factory = Arc::clone(&self.factory);
        let sink = Arc::clone(&self.sink);
        let worker_count = Arc::clone(&self.workers);
        let worker = std::thread::Builder::new()
            .name(format!("clipline-mic-monitor-{generation}"))
            .spawn(move || {
                let _worker = ActiveWorkerGuard::enter(worker_count);
                let Ok(StartGate::Start) = gate_rx.recv() else {
                    return;
                };
                run_worker(generation, config, worker_stop, factory, sink);
            })
            .map_err(|error| MicrophoneServiceError::Spawn(error.to_string()))?;
        let replacement = MicrophoneSession {
            generation,
            stop,
            gate,
            worker,
        };

        let previous = self
            .state
            .lock()
            .map_err(|_| MicrophoneServiceError::Poisoned)?
            .active
            .take();
        if let Some(previous) = previous {
            if let Err(error) = previous.cancel_and_join() {
                let _ = replacement.cancel_and_join();
                return Err(error);
            }
        }

        let mut state = self
            .state
            .lock()
            .map_err(|_| MicrophoneServiceError::Poisoned)?;
        state.active = Some(replacement);
        let active = state
            .active
            .as_ref()
            .expect("replacement was installed before its start gate opens");
        if active.gate.send(StartGate::Start).is_err() {
            let failed = state.active.take().expect("failed active session exists");
            drop(state);
            let _ = failed.cancel_and_join();
            return Err(MicrophoneServiceError::WorkerPanicked);
        }
        Ok(generation)
    }

    pub fn stop(&self) -> Result<(), MicrophoneServiceError> {
        let _operation = self
            .operation
            .lock()
            .map_err(|_| MicrophoneServiceError::Poisoned)?;
        let active = self
            .state
            .lock()
            .map_err(|_| MicrophoneServiceError::Poisoned)?
            .active
            .take();
        active.map_or(Ok(()), MicrophoneSession::cancel_and_join)
    }

    /// Permanently prevents later admission and joins the current worker.
    pub fn shutdown(&self) -> Result<(), MicrophoneServiceError> {
        let _operation = self
            .operation
            .lock()
            .map_err(|_| MicrophoneServiceError::Poisoned)?;
        let active = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| MicrophoneServiceError::Poisoned)?;
            state.shut_down = true;
            state.active.take()
        };
        active.map_or(Ok(()), MicrophoneSession::cancel_and_join)
    }

    #[must_use]
    pub fn active_generation(&self) -> Option<u64> {
        self.state.lock().ok().and_then(|state| {
            state
                .active
                .as_ref()
                .filter(|active| !active.worker.is_finished())
                .map(|active| active.generation)
        })
    }

    #[must_use]
    pub fn worker_count(&self) -> usize {
        self.workers.load(Ordering::Acquire)
    }
}

impl Drop for MicrophoneMonitorService {
    fn drop(&mut self) {
        let state = self
            .state
            .get_mut()
            .unwrap_or_else(|poison| poison.into_inner());
        let active = state.active.take();
        if let Some(active) = active {
            let _ = active.cancel_and_join();
        }
    }
}

#[must_use]
pub fn active_microphone_workers() -> usize {
    ACTIVE_MICROPHONE_WORKERS.load(Ordering::Acquire)
}

struct ActiveWorkerGuard {
    service: Arc<AtomicUsize>,
}

impl ActiveWorkerGuard {
    fn enter(service: Arc<AtomicUsize>) -> Self {
        ACTIVE_MICROPHONE_WORKERS.fetch_add(1, Ordering::AcqRel);
        service.fetch_add(1, Ordering::AcqRel);
        Self { service }
    }
}

impl Drop for ActiveWorkerGuard {
    fn drop(&mut self) {
        self.service.fetch_sub(1, Ordering::AcqRel);
        ACTIVE_MICROPHONE_WORKERS.fetch_sub(1, Ordering::AcqRel);
    }
}

fn run_worker(
    generation: u64,
    config: MicrophoneMonitorConfig,
    stop: MicrophoneStopToken,
    factory: Arc<dyn MicrophoneMonitorFactory>,
    sink: Arc<dyn MicrophoneMonitorEventSink>,
) {
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        let mut source = factory.open_source(&config, &stop)?;
        if stop.is_stopped() {
            return Ok(());
        }
        let mut renderer = if config.output == MicrophoneOutputMode::NativeRenderer {
            Some(factory.open_renderer(generation, &stop)?)
        } else {
            None
        };
        if stop.is_stopped() {
            return Ok(());
        }

        let mut samples = Vec::with_capacity(MAX_MICROPHONE_MONITOR_SAMPLES);
        loop {
            if stop.wait_or_stopped(MONITOR_POLL_INTERVAL) {
                break;
            }
            samples.clear();
            source.poll_48khz_stereo(&mut samples)?;
            validate_samples(&samples)?;
            if stop.is_stopped() {
                break;
            }
            let (rms, peak) = levels(&samples);
            if let Some(renderer) = &mut renderer {
                let accepted = renderer.write_48khz_stereo(&samples)?;
                let requested = samples.len() / MICROPHONE_CHANNELS;
                if accepted > requested {
                    return Err("microphone renderer accepted more frames than supplied".into());
                }
            }
            if stop.is_stopped() {
                break;
            }
            let pcm_i16 = (config.output == MicrophoneOutputMode::TauriCompatibilityPcm)
                .then(|| samples.iter().copied().map(float_to_i16).collect());
            let _ = sink.try_publish(MicrophoneMonitorEvent::Monitor {
                generation,
                rms,
                peak,
                sample_count: samples.len(),
                pcm_i16,
            });
        }
        Ok::<(), String>(())
    }));

    let error = match outcome {
        Ok(Ok(())) => None,
        Ok(Err(error)) => Some(bounded_error(error)),
        Err(_) => Some("microphone monitor worker panicked".into()),
    };
    if let Some(message) = error.filter(|_| !stop.is_stopped()) {
        let _ = sink.try_publish(MicrophoneMonitorEvent::Error {
            generation,
            message,
        });
    }
    let _ = sink.try_publish(MicrophoneMonitorEvent::Stopped { generation });
}

fn validate_samples(samples: &[f32]) -> Result<(), String> {
    if samples.len() > MAX_MICROPHONE_MONITOR_SAMPLES {
        return Err(format!(
            "microphone source returned {} samples; maximum is {MAX_MICROPHONE_MONITOR_SAMPLES}",
            samples.len()
        ));
    }
    if !samples.len().is_multiple_of(MICROPHONE_CHANNELS) {
        return Err("microphone source returned an incomplete stereo frame".into());
    }
    if samples.iter().any(|sample| !sample.is_finite()) {
        return Err("microphone source returned a non-finite sample".into());
    }
    Ok(())
}

fn levels(samples: &[f32]) -> (f32, f32) {
    if samples.is_empty() {
        return (0.0, 0.0);
    }
    let mut sum_squares = 0.0f64;
    let mut peak = 0.0f32;
    for sample in samples.iter().copied() {
        let clamped = sample.clamp(-1.0, 1.0);
        sum_squares += f64::from(clamped) * f64::from(clamped);
        peak = peak.max(clamped.abs());
    }
    ((sum_squares / samples.len() as f64).sqrt() as f32, peak)
}

fn float_to_i16(sample: f32) -> i16 {
    let scaled = (sample.clamp(-1.0, 1.0) * 32_768.0).round();
    scaled.clamp(i16::MIN as f32, i16::MAX as f32) as i16
}

fn bounded_error(mut error: String) -> String {
    if error.len() <= MAX_MICROPHONE_ERROR_BYTES {
        return error;
    }
    let mut boundary = MAX_MICROPHONE_ERROR_BYTES;
    while boundary > 0 && !error.is_char_boundary(boundary) {
        boundary -= 1;
    }
    error.truncate(boundary);
    error
}

#[cfg(windows)]
mod windows_backend {
    use clipline_capture::clock::RelativeClock;
    use clipline_capture::windows::qpc_now_ticks_100ns;
    use clipline_capture::windows::wasapi::{WasapiChannelMode, WasapiMicrophoneMonitor};
    use clipline_playback::windows::WindowsWasapiRenderer;
    use clipline_playback::{AudioRenderer, PipelineToken, RecoveryDisposition, WorkGeneration};

    use super::*;

    #[derive(Debug, Default)]
    pub struct WindowsMicrophoneMonitorFactory;

    struct WindowsMicrophoneSource {
        source: WasapiMicrophoneMonitor,
    }

    impl MicrophoneMonitorSource for WindowsMicrophoneSource {
        fn poll_48khz_stereo(&mut self, samples: &mut Vec<f32>) -> Result<(), String> {
            samples.extend_from_slice(
                self.source
                    .poll_samples()
                    .map_err(|error| error.to_string())?,
            );
            Ok(())
        }
    }

    struct WindowsMicrophoneRenderer {
        renderer: WindowsWasapiRenderer,
        token: PipelineToken,
        next_default_endpoint_check: Instant,
    }

    impl WindowsMicrophoneRenderer {
        fn open(generation: u64) -> Result<Self, String> {
            let token = PipelineToken::new(WorkGeneration::new(generation, 0), 0);
            let mut renderer =
                WindowsWasapiRenderer::open_default().map_err(|error| error.to_string())?;
            renderer.reset(token).map_err(|error| error.to_string())?;
            renderer.start(token).map_err(|error| error.to_string())?;
            Ok(Self {
                renderer,
                token,
                next_default_endpoint_check: Instant::now()
                    + DEFAULT_RENDER_ENDPOINT_RECHECK_INTERVAL,
            })
        }

        fn write_once(
            &mut self,
            samples: &[f32],
        ) -> Result<usize, clipline_playback::BackendError> {
            self.renderer.write_stereo_frames(samples, self.token)
        }

        fn reopen(&mut self) -> Result<(), String> {
            self.renderer
                .reopen(self.token)
                .map_err(|error| error.to_string())?;
            self.renderer
                .start(self.token)
                .map_err(|error| error.to_string())
        }
    }

    impl MicrophoneMonitorRenderer for WindowsMicrophoneRenderer {
        fn write_48khz_stereo(&mut self, samples: &[f32]) -> Result<usize, String> {
            let now = Instant::now();
            if now >= self.next_default_endpoint_check {
                self.next_default_endpoint_check = now + DEFAULT_RENDER_ENDPOINT_RECHECK_INTERVAL;
                if self
                    .renderer
                    .default_endpoint_changed()
                    .map_err(|error| error.to_string())?
                {
                    self.reopen()?;
                }
            }
            match self.write_once(samples) {
                Ok(written) => Ok(written),
                Err(error) if error.recovery == RecoveryDisposition::RecreateComponent => {
                    self.reopen()?;
                    self.write_once(samples).map_err(|error| error.to_string())
                }
                Err(error) => Err(error.to_string()),
            }
        }
    }

    impl MicrophoneMonitorFactory for WindowsMicrophoneMonitorFactory {
        fn open_source(
            &self,
            config: &MicrophoneMonitorConfig,
            stop: &MicrophoneStopToken,
        ) -> Result<Box<dyn MicrophoneMonitorSource>, String> {
            if stop.is_stopped() {
                return Err("microphone monitor canceled before capture activation".into());
            }
            let clock =
                RelativeClock::new(qpc_now_ticks_100ns().map_err(|error| error.to_string())?);
            let channels = if config.mono {
                WasapiChannelMode::Mono
            } else {
                WasapiChannelMode::Stereo
            };
            let source = WasapiMicrophoneMonitor::start(
                clock,
                config.device_id.as_deref(),
                f64::from(config.volume),
                channels,
            )
            .map_err(|error| error.to_string())?;
            if stop.is_stopped() {
                return Err("microphone monitor canceled during capture activation".into());
            }
            Ok(Box::new(WindowsMicrophoneSource { source }))
        }

        fn open_renderer(
            &self,
            generation: u64,
            stop: &MicrophoneStopToken,
        ) -> Result<Box<dyn MicrophoneMonitorRenderer>, String> {
            if stop.is_stopped() {
                return Err("microphone monitor canceled before renderer activation".into());
            }
            let renderer = WindowsMicrophoneRenderer::open(generation)?;
            if stop.is_stopped() {
                return Err("microphone monitor canceled during renderer activation".into());
            }
            Ok(Box::new(renderer))
        }
    }
}

#[cfg(windows)]
pub use windows_backend::WindowsMicrophoneMonitorFactory;
