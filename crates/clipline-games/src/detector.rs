//! Process-owned, generation-fenced active-game detector lifecycle.

use std::error::Error;
use std::fmt;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use clipline_settings::games::{validate_custom_game_id, GameSettings};
use clipline_settings::{
    MAX_SETTINGS_COLLECTION_BYTES, MAX_SETTINGS_CUSTOM_GAMES, MAX_SETTINGS_FIELD_BYTES,
    MAX_SETTINGS_GAME_PLUGINS,
};

use crate::detection::DetectedGame;

pub const MAX_GAME_DETECTOR_ERROR_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GameDetectorGeneration(u64);

impl GameDetectorGeneration {
    const INITIAL: Self = Self(1);

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GameDetectorWorkEpoch(u64);

impl GameDetectorWorkEpoch {
    const INITIAL: Self = Self(1);

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GameDetectorToken {
    pub generation: GameDetectorGeneration,
    pub work_epoch: GameDetectorWorkEpoch,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GameDetectorCheckpointError {
    Stale,
    Quiesced,
    ShutDown,
}

impl fmt::Display for GameDetectorCheckpointError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stale => formatter.write_str("game detector work is stale"),
            Self::Quiesced => formatter.write_str("game detector is quiesced"),
            Self::ShutDown => formatter.write_str("game detector is shut down"),
        }
    }
}

impl Error for GameDetectorCheckpointError {}

#[derive(Clone)]
pub struct GameDetectorCheckpoint {
    shared: Arc<Shared>,
    token: GameDetectorToken,
}

impl GameDetectorCheckpoint {
    #[must_use]
    pub fn token(&self) -> GameDetectorToken {
        self.token
    }

    #[must_use]
    pub fn is_current(&self) -> bool {
        self.check().is_ok()
    }

    pub fn check(&self) -> Result<(), GameDetectorCheckpointError> {
        let state = lock_recover(&self.shared.state);
        if state.shut_down {
            Err(GameDetectorCheckpointError::ShutDown)
        } else if state.quiesced {
            Err(GameDetectorCheckpointError::Quiesced)
        } else if state.active.generation == self.token.generation
            && state.work_epoch == self.token.work_epoch
        {
            Ok(())
        } else {
            Err(GameDetectorCheckpointError::Stale)
        }
    }
}

pub trait GameDetectionProbe: Send + Sync + 'static {
    /// Perform one bounded detection pass. Implementations performing more
    /// than one OS step must call `checkpoint` between those steps.
    fn detect(
        &self,
        settings: &GameSettings,
        checkpoint: &GameDetectorCheckpoint,
    ) -> Result<Option<DetectedGame>, String>;
}

pub enum GameDetectionEvent {
    Detection {
        token: GameDetectorToken,
        detected: Option<DetectedGame>,
    },
    Failed {
        token: GameDetectorToken,
        message: String,
    },
}

impl GameDetectionEvent {
    #[must_use]
    pub const fn token(&self) -> GameDetectorToken {
        match self {
            Self::Detection { token, .. } | Self::Failed { token, .. } => *token,
        }
    }
}

impl fmt::Debug for GameDetectionEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Detection { token, detected } => formatter
                .debug_struct("GameDetectionEvent::Detection")
                .field("token", token)
                .field("active", &detected.is_some())
                .finish(),
            Self::Failed { token, message } => formatter
                .debug_struct("GameDetectionEvent::Failed")
                .field("token", token)
                .field("message_bytes", &message.len())
                .finish(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GameDetectorSinkError {
    Full,
    Disconnected,
    Failed(String),
}

pub struct RejectedGameDetectionEvent {
    pub error: GameDetectorSinkError,
    pub event: GameDetectionEvent,
}

impl fmt::Debug for RejectedGameDetectionEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RejectedGameDetectionEvent")
            .field("error", &self.error)
            .field("event", &self.event)
            .finish()
    }
}

pub trait GameDetectionSink: Send + Sync + 'static {
    /// On rejection, return the exact move-owned event. `Full` is retryable;
    /// `Disconnected` permanently stops publication until service shutdown.
    fn try_publish(&self, event: GameDetectionEvent)
        -> Result<(), Box<RejectedGameDetectionEvent>>;
}

pub trait GameDetectorThreadSpawner: Send + Sync {
    fn spawn(&self, name: &str, task: Box<dyn FnOnce() + Send>) -> Result<JoinHandle<()>, String>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemGameDetectorThreadSpawner;

impl GameDetectorThreadSpawner for SystemGameDetectorThreadSpawner {
    fn spawn(&self, name: &str, task: Box<dyn FnOnce() + Send>) -> Result<JoinHandle<()>, String> {
        std::thread::Builder::new()
            .name(name.into())
            .spawn(task)
            .map_err(|error| error.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GameDetectorServiceError {
    InvalidConfig(String),
    GenerationExhausted,
    WorkEpochExhausted,
    PreparationPending,
    ShutDown,
    Spawn(String),
    WorkerPanicked,
}

impl fmt::Display for GameDetectorServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig(error) => {
                write!(formatter, "invalid game detector config: {error}")
            }
            Self::GenerationExhausted => formatter.write_str("game detector generation exhausted"),
            Self::WorkEpochExhausted => formatter.write_str("game detector work epoch exhausted"),
            Self::PreparationPending => {
                formatter.write_str("a game detector reconfiguration is already prepared")
            }
            Self::ShutDown => formatter.write_str("game detector service is shut down"),
            Self::Spawn(error) => {
                write!(formatter, "could not start game detector worker: {error}")
            }
            Self::WorkerPanicked => formatter.write_str("game detector worker panicked"),
        }
    }
}

impl Error for GameDetectorServiceError {}

struct ActiveConfig {
    generation: GameDetectorGeneration,
    settings: Arc<GameSettings>,
}

struct SharedState {
    last_generation: GameDetectorGeneration,
    work_epoch: GameDetectorWorkEpoch,
    prepared_generation: Option<GameDetectorGeneration>,
    active: ActiveConfig,
    quiesced: bool,
    shut_down: bool,
}

struct Shared {
    state: Mutex<SharedState>,
    publication: Mutex<()>,
    changed: Condvar,
}

struct DetectorCore {
    shared: Arc<Shared>,
    worker: Mutex<Option<JoinHandle<()>>>,
    worker_count: Arc<AtomicUsize>,
}

impl DetectorCore {
    fn shutdown(&self) -> Result<(), GameDetectorServiceError> {
        {
            let _publication = lock_recover(&self.shared.publication);
            let mut state = lock_recover(&self.shared.state);
            if state.prepared_generation.is_some() {
                return Err(GameDetectorServiceError::PreparationPending);
            }
            state.shut_down = true;
            self.shared.changed.notify_all();
        }
        let worker = lock_recover(&self.worker).take();
        worker.map_or(Ok(()), |worker| {
            worker
                .join()
                .map_err(|_| GameDetectorServiceError::WorkerPanicked)
        })
    }
}

impl Drop for DetectorCore {
    fn drop(&mut self) {
        {
            let _publication = lock_recover(&self.shared.publication);
            let mut state = lock_recover(&self.shared.state);
            state.shut_down = true;
            self.shared.changed.notify_all();
        }
        if let Some(worker) = self
            .worker
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            let _ = worker.join();
        }
    }
}

pub struct PreparedDetectorReconfiguration {
    core: Arc<DetectorCore>,
    generation: GameDetectorGeneration,
    settings: Option<Arc<GameSettings>>,
}

impl PreparedDetectorReconfiguration {
    #[must_use]
    pub const fn generation(&self) -> GameDetectorGeneration {
        self.generation
    }

    /// Commit is allocation-free and infallible while this exact receipt is
    /// live: shutdown refuses outstanding preparations and only one receipt
    /// may exist at a time.
    #[must_use]
    pub fn commit(mut self) -> GameDetectorToken {
        let settings = self
            .settings
            .take()
            .expect("prepared detector receipt owns its settings");
        let _publication = lock_recover(&self.core.shared.publication);
        let mut state = lock_recover(&self.core.shared.state);
        assert_eq!(
            state.prepared_generation,
            Some(self.generation),
            "prepared detector generation changed while its receipt was live"
        );
        assert!(
            !state.shut_down,
            "prepared detector committed after shutdown"
        );
        state.active = ActiveConfig {
            generation: self.generation,
            settings,
        };
        state.prepared_generation = None;
        let token = GameDetectorToken {
            generation: self.generation,
            work_epoch: state.work_epoch,
        };
        self.core.shared.changed.notify_all();
        token
    }
}

impl Drop for PreparedDetectorReconfiguration {
    fn drop(&mut self) {
        if self.settings.is_none() {
            return;
        }
        let mut state = lock_recover(&self.core.shared.state);
        if state.prepared_generation == Some(self.generation) {
            state.prepared_generation = None;
            self.core.shared.changed.notify_all();
        }
    }
}

#[derive(Clone)]
pub struct GameDetectorService {
    core: Arc<DetectorCore>,
}

impl GameDetectorService {
    pub fn start<P, S>(
        initial: GameSettings,
        interval: Duration,
        probe: Arc<P>,
        sink: Arc<S>,
    ) -> Result<Self, GameDetectorServiceError>
    where
        P: GameDetectionProbe,
        S: GameDetectionSink,
    {
        Self::start_with_spawner(
            initial,
            interval,
            probe,
            sink,
            &SystemGameDetectorThreadSpawner,
        )
    }

    pub fn start_with_spawner<P, S>(
        initial: GameSettings,
        interval: Duration,
        probe: Arc<P>,
        sink: Arc<S>,
        spawner: &dyn GameDetectorThreadSpawner,
    ) -> Result<Self, GameDetectorServiceError>
    where
        P: GameDetectionProbe,
        S: GameDetectionSink,
    {
        if interval.is_zero() {
            return Err(GameDetectorServiceError::InvalidConfig(
                "poll interval must be nonzero".into(),
            ));
        }
        validate_detector_settings(&initial).map_err(GameDetectorServiceError::InvalidConfig)?;
        let shared = Arc::new(Shared {
            state: Mutex::new(SharedState {
                last_generation: GameDetectorGeneration::INITIAL,
                work_epoch: GameDetectorWorkEpoch::INITIAL,
                prepared_generation: None,
                active: ActiveConfig {
                    generation: GameDetectorGeneration::INITIAL,
                    settings: Arc::new(initial),
                },
                quiesced: false,
                shut_down: false,
            }),
            publication: Mutex::new(()),
            changed: Condvar::new(),
        });
        let worker_count = Arc::new(AtomicUsize::new(0));
        let worker_shared = Arc::clone(&shared);
        let worker_counter = Arc::clone(&worker_count);
        let worker = spawner
            .spawn(
                "clipline-game-detector",
                Box::new(move || run_worker(worker_shared, interval, probe, sink, worker_counter)),
            )
            .map_err(GameDetectorServiceError::Spawn)?;
        Ok(Self {
            core: Arc::new(DetectorCore {
                shared,
                worker: Mutex::new(Some(worker)),
                worker_count,
            }),
        })
    }

    pub fn prepare_reconfiguration(
        &self,
        settings: GameSettings,
    ) -> Result<PreparedDetectorReconfiguration, GameDetectorServiceError> {
        validate_detector_settings(&settings).map_err(GameDetectorServiceError::InvalidConfig)?;
        let mut state = lock_recover(&self.core.shared.state);
        if state.shut_down {
            return Err(GameDetectorServiceError::ShutDown);
        }
        if state.prepared_generation.is_some() {
            return Err(GameDetectorServiceError::PreparationPending);
        }
        let generation = next_generation(state.last_generation)?;
        state.last_generation = generation;
        state.prepared_generation = Some(generation);
        drop(state);
        Ok(PreparedDetectorReconfiguration {
            core: Arc::clone(&self.core),
            generation,
            settings: Some(Arc::new(settings)),
        })
    }

    #[must_use]
    pub fn active_generation(&self) -> GameDetectorGeneration {
        lock_recover(&self.core.shared.state).active.generation
    }

    #[must_use]
    pub fn active_token(&self) -> GameDetectorToken {
        let state = lock_recover(&self.core.shared.state);
        GameDetectorToken {
            generation: state.active.generation,
            work_epoch: state.work_epoch,
        }
    }

    /// Stop admission at the event/recorder-intent boundary. This waits for
    /// an already-entered sink call, cancels probe work at its next
    /// checkpoint, and keeps the latest committed configuration for resume.
    pub fn quiesce(&self) -> Result<(), GameDetectorServiceError> {
        let _publication = lock_recover(&self.core.shared.publication);
        let mut state = lock_recover(&self.core.shared.state);
        if state.shut_down {
            return Err(GameDetectorServiceError::ShutDown);
        }
        if !state.quiesced {
            state.work_epoch = next_work_epoch(state.work_epoch)?;
            state.quiesced = true;
        }
        self.core.shared.changed.notify_all();
        Ok(())
    }

    pub fn resume(&self) -> Result<(), GameDetectorServiceError> {
        let mut state = lock_recover(&self.core.shared.state);
        if state.shut_down {
            return Err(GameDetectorServiceError::ShutDown);
        }
        state.quiesced = false;
        self.core.shared.changed.notify_all();
        Ok(())
    }

    pub fn shutdown(&self) -> Result<(), GameDetectorServiceError> {
        self.core.shutdown()
    }

    #[must_use]
    pub fn worker_count(&self) -> usize {
        self.core.worker_count.load(Ordering::Acquire)
    }
}

impl Drop for GameDetectorService {
    fn drop(&mut self) {
        if Arc::strong_count(&self.core) == 1 {
            let _ = self.core.shutdown();
        }
    }
}

fn run_worker<P, S>(
    shared: Arc<Shared>,
    interval: Duration,
    probe: Arc<P>,
    sink: Arc<S>,
    worker_count: Arc<AtomicUsize>,
) where
    P: GameDetectionProbe,
    S: GameDetectionSink,
{
    let _active = ActiveWorkerGuard::enter(worker_count);
    let mut pending = None::<GameDetectionEvent>;
    let mut last_token = None::<GameDetectorToken>;
    let mut last_probe_error = None::<String>;
    let mut last_sink_error = None::<String>;
    loop {
        let (token, settings) = {
            let mut state = lock_recover(&shared.state);
            while state.quiesced && !state.shut_down {
                state = shared
                    .changed
                    .wait(state)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
            if state.shut_down {
                return;
            }
            (
                GameDetectorToken {
                    generation: state.active.generation,
                    work_epoch: state.work_epoch,
                },
                Arc::clone(&state.active.settings),
            )
        };
        if last_token != Some(token) {
            last_token = Some(token);
            last_probe_error = None;
            last_sink_error = None;
        }
        let checkpoint = GameDetectorCheckpoint {
            shared: Arc::clone(&shared),
            token,
        };

        let event = match pending.take() {
            Some(event) if event.token() == token => event,
            Some(_) | None => {
                if checkpoint.check().is_err() {
                    continue;
                }
                let detected =
                    catch_unwind(AssertUnwindSafe(|| probe.detect(&settings, &checkpoint)));
                if checkpoint.check().is_err() {
                    continue;
                }
                match detected {
                    Ok(Ok(detected)) => match validate_detected_game(detected.as_ref()) {
                        Ok(()) => {
                            last_probe_error = None;
                            GameDetectionEvent::Detection { token, detected }
                        }
                        Err(error) => {
                            if last_probe_error.as_deref() == Some(error.as_str()) {
                                wait_for_poll(&shared, token, interval);
                                continue;
                            }
                            last_probe_error = Some(error.clone());
                            GameDetectionEvent::Failed {
                                token,
                                message: error,
                            }
                        }
                    },
                    Ok(Err(error)) => {
                        let error = bounded_error(error);
                        if last_probe_error.as_deref() == Some(error.as_str()) {
                            wait_for_poll(&shared, token, interval);
                            continue;
                        }
                        last_probe_error = Some(error.clone());
                        GameDetectionEvent::Failed {
                            token,
                            message: error,
                        }
                    }
                    Err(_) => {
                        let error = "game detector probe panicked".to_owned();
                        if last_probe_error.as_deref() == Some(error.as_str()) {
                            wait_for_poll(&shared, token, interval);
                            continue;
                        }
                        last_probe_error = Some(error.clone());
                        GameDetectionEvent::Failed {
                            token,
                            message: error,
                        }
                    }
                }
            }
        };

        let publishes_detection = matches!(&event, GameDetectionEvent::Detection { .. });
        let published = {
            let _publication = lock_recover(&shared.publication);
            if checkpoint.check().is_err() {
                continue;
            }
            catch_unwind(AssertUnwindSafe(|| sink.try_publish(event)))
        };
        match published {
            Ok(Ok(())) => {
                if publishes_detection {
                    last_sink_error = None;
                }
                wait_for_poll(&shared, token, interval);
            }
            Ok(Err(rejected)) => match rejected.error {
                GameDetectorSinkError::Full => {
                    pending = Some(rejected.event);
                    wait_for_poll(&shared, token, interval);
                }
                GameDetectorSinkError::Disconnected => {
                    wait_until_shutdown(&shared);
                    return;
                }
                GameDetectorSinkError::Failed(error) => {
                    let error = bounded_error(error);
                    if last_sink_error.as_deref() != Some(error.as_str()) {
                        last_sink_error = Some(error.clone());
                        pending = Some(GameDetectionEvent::Failed {
                            token,
                            message: error,
                        });
                    }
                    wait_for_poll(&shared, token, interval);
                }
            },
            Err(_) => {
                let error = "game detector sink panicked".to_owned();
                if last_sink_error.as_deref() != Some(error.as_str()) {
                    last_sink_error = Some(error.clone());
                    pending = Some(GameDetectionEvent::Failed {
                        token,
                        message: error,
                    });
                }
                wait_for_poll(&shared, token, interval);
            }
        }
    }
}

fn wait_for_poll(shared: &Shared, token: GameDetectorToken, interval: Duration) {
    let state = lock_recover(&shared.state);
    if state.shut_down
        || state.active.generation != token.generation
        || state.work_epoch != token.work_epoch
    {
        return;
    }
    let _ = shared.changed.wait_timeout(state, interval);
}

fn wait_until_shutdown(shared: &Shared) {
    let mut state = lock_recover(&shared.state);
    while !state.shut_down {
        state = shared
            .changed
            .wait(state)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
    }
}

fn bounded_error(mut error: String) -> String {
    if error.len() <= MAX_GAME_DETECTOR_ERROR_BYTES {
        return error;
    }
    let mut boundary = MAX_GAME_DETECTOR_ERROR_BYTES;
    while !error.is_char_boundary(boundary) {
        boundary -= 1;
    }
    error.truncate(boundary);
    error
}

fn validate_detector_settings(settings: &GameSettings) -> Result<(), String> {
    if settings.plugins.len() > MAX_SETTINGS_GAME_PLUGINS {
        return Err(format!(
            "game plugin count exceeds {MAX_SETTINGS_GAME_PLUGINS}"
        ));
    }
    if settings.custom_games.len() > MAX_SETTINGS_CUSTOM_GAMES {
        return Err(format!(
            "custom game count exceeds {MAX_SETTINGS_CUSTOM_GAMES}"
        ));
    }
    let mut aggregate = 0usize;
    for plugin_id in settings.plugins.keys() {
        account_detector_text(&mut aggregate, plugin_id, MAX_SETTINGS_FIELD_BYTES)?;
    }
    for (index, game) in settings.custom_games.iter().enumerate() {
        account_detector_text(&mut aggregate, &game.id, MAX_SETTINGS_FIELD_BYTES)?;
        validate_custom_game_id(&game.id)?;
        if settings.custom_games[..index]
            .iter()
            .any(|previous| previous.id.eq_ignore_ascii_case(&game.id))
        {
            return Err("custom game ids must be unique".into());
        }
        if game.legacy_ids.len() > 8 {
            return Err("custom game legacy id count exceeds 8".into());
        }
        for legacy_id in &game.legacy_ids {
            account_detector_text(&mut aggregate, legacy_id, 256)?;
        }
        account_detector_text(&mut aggregate, &game.name, MAX_SETTINGS_FIELD_BYTES)?;
        account_detector_text(&mut aggregate, &game.exe_name, MAX_SETTINGS_FIELD_BYTES)?;
        if let Some(path) = game.process_path.as_deref() {
            account_detector_text(&mut aggregate, path, MAX_SETTINGS_FIELD_BYTES)?;
        }
        account_detector_text(&mut aggregate, &game.window_title, MAX_SETTINGS_FIELD_BYTES)?;
        if let Some(icon) = game.icon.as_deref() {
            account_detector_text(&mut aggregate, icon, MAX_SETTINGS_COLLECTION_BYTES)?;
        }
        if game.name.trim().is_empty() || !game.has_match_identity() {
            return Err("custom game requires a name and match identity".into());
        }
    }
    Ok(())
}

fn validate_detected_game(detected: Option<&DetectedGame>) -> Result<(), String> {
    let Some(detected) = detected else {
        return Ok(());
    };
    let mut aggregate = 0usize;
    for text in [
        detected.identity.id(),
        detected.name.as_str(),
        detected.window_title.as_str(),
        detected.exe_name.as_str(),
    ] {
        account_detector_text(&mut aggregate, text, MAX_SETTINGS_FIELD_BYTES)?;
    }
    Ok(())
}

fn account_detector_text(aggregate: &mut usize, text: &str, maximum: usize) -> Result<(), String> {
    if text.len() > maximum {
        return Err("game detector text exceeds its byte bound".into());
    }
    *aggregate = aggregate
        .checked_add(text.len())
        .ok_or_else(|| "game detector text accounting overflowed".to_string())?;
    if *aggregate > MAX_SETTINGS_COLLECTION_BYTES {
        return Err("game detector config exceeds its aggregate byte bound".into());
    }
    Ok(())
}

fn next_generation(
    current: GameDetectorGeneration,
) -> Result<GameDetectorGeneration, GameDetectorServiceError> {
    current
        .0
        .checked_add(1)
        .map(GameDetectorGeneration)
        .ok_or(GameDetectorServiceError::GenerationExhausted)
}

fn next_work_epoch(
    current: GameDetectorWorkEpoch,
) -> Result<GameDetectorWorkEpoch, GameDetectorServiceError> {
    current
        .0
        .checked_add(1)
        .map(GameDetectorWorkEpoch)
        .ok_or(GameDetectorServiceError::WorkEpochExhausted)
}

fn lock_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct ActiveWorkerGuard(Arc<AtomicUsize>);

impl ActiveWorkerGuard {
    fn enter(count: Arc<AtomicUsize>) -> Self {
        count.fetch_add(1, Ordering::AcqRel);
        Self(count)
    }
}

impl Drop for ActiveWorkerGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_exhaustion_is_atomic() {
        assert_eq!(
            next_generation(GameDetectorGeneration(u64::MAX)),
            Err(GameDetectorServiceError::GenerationExhausted)
        );
    }

    #[test]
    fn work_epoch_exhaustion_is_atomic() {
        assert_eq!(
            next_work_epoch(GameDetectorWorkEpoch(u64::MAX)),
            Err(GameDetectorServiceError::WorkEpochExhausted)
        );
    }
}
