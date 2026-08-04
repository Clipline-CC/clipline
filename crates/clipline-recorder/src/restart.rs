//! Start-latched, join-owned recorder worker lifecycle.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvError, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use crate::{Cmd, Event, ServiceOptions};

const PARKED: u8 = 0;
const START: u8 = 1;
const CANCEL: u8 = 2;

static ACTIVE_RECORDER_WORKERS: AtomicUsize = AtomicUsize::new(0);

struct ActiveWorkerGuard;

impl Drop for ActiveWorkerGuard {
    fn drop(&mut self) {
        ACTIVE_RECORDER_WORKERS.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Number of recorder worker threads owned by this process, including parked
/// settings-restart workers. Exposed for lifecycle diagnostics and soak tests.
pub fn active_recorder_workers() -> usize {
    ACTIVE_RECORDER_WORKERS.load(Ordering::Acquire)
}

/// A recorder worker which has been created but cannot touch capture, D3D,
/// audio, or storage until [`Self::commit`] releases its start latch.
pub struct PreparedRecorderRestart {
    state: Arc<AtomicU8>,
    commands: Sender<Cmd>,
    events: Option<Receiver<Event>>,
    worker: Option<JoinHandle<()>>,
    options: Option<Arc<Mutex<ServiceOptions>>>,
}

impl PreparedRecorderRestart {
    pub fn prepare(options: ServiceOptions) -> Result<Self, String> {
        validate_options(&options)?;
        let options = Arc::new(Mutex::new(options));
        let worker_options = options.clone();
        let mut prepared = Self::prepare_with(move |commands, events| {
            let options = worker_options
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone();
            if let Err(error) = crate::service::run(options, commands, &events) {
                let _ = events.send(Event::Error { message: error });
                crate::service::send_stopped(&events);
            }
        })?;
        prepared.options = Some(options);
        Ok(prepared)
    }

    fn prepare_with(
        run: impl FnOnce(Receiver<Cmd>, Sender<Event>) + Send + 'static,
    ) -> Result<Self, String> {
        let (command_tx, command_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::sync_channel(0);
        let state = Arc::new(AtomicU8::new(PARKED));
        let worker_state = state.clone();
        let worker = std::thread::Builder::new()
            .name("clipline-recorder".into())
            .spawn(move || {
                ACTIVE_RECORDER_WORKERS.fetch_add(1, Ordering::AcqRel);
                let _active = ActiveWorkerGuard;
                let _ = ready_tx.send(());
                loop {
                    match worker_state.load(Ordering::Acquire) {
                        START => break,
                        CANCEL => return,
                        PARKED => std::thread::park(),
                        _ => return,
                    }
                }
                if catch_unwind(AssertUnwindSafe(|| run(command_rx, event_tx.clone()))).is_err() {
                    let _ = event_tx.send(Event::Error {
                        message: "recorder worker panicked".into(),
                    });
                    crate::service::send_stopped(&event_tx);
                }
            })
            .map_err(|error| format!("spawn recorder thread: {error}"))?;
        if ready_rx.recv().is_err() {
            let _ = worker.join();
            return Err("recorder worker exited before reaching its start latch".into());
        }
        Ok(Self {
            state,
            commands: command_tx,
            events: Some(event_rx),
            worker: Some(worker),
            options: None,
        })
    }

    /// Clone the parked worker's command sender for atomic publication before
    /// the start latch is released.
    pub fn command_sender(&self) -> Sender<Cmd> {
        self.commands.clone()
    }

    /// Release the start latch and transfer join ownership to the event pump.
    /// All fallible allocation, validation, and thread creation happened in
    /// [`Self::prepare`], so this operation is structurally infallible.
    pub fn commit(mut self) -> RecorderEventStream {
        self.state.store(START, Ordering::Release);
        if let Some(worker) = self.worker.as_ref() {
            worker.thread().unpark();
        }
        RecorderEventStream {
            receiver: self
                .events
                .take()
                .expect("prepared recorder owns one event receiver"),
            stop: self.commands.clone(),
            worker: self.worker.take(),
        }
    }

    /// Replace only the already-validated runtime context immediately before
    /// releasing the start latch. Settings restart adapters use this to fold
    /// in the latest game/window ownership without doing fallible work after
    /// durable preferences have committed.
    pub fn commit_with_options(self, options: ServiceOptions) -> RecorderEventStream {
        if let Some(slot) = self.options.as_ref() {
            *slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = options;
        }
        self.commit()
    }

    /// Explicit pre-commit cancellation. Drop provides the same guarantee.
    pub fn discard(self) {}
}

impl Drop for PreparedRecorderRestart {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            self.state.store(CANCEL, Ordering::Release);
            worker.thread().unpark();
            let _ = worker.join();
        }
    }
}

/// Recorder events plus exclusive ownership of the worker thread.
pub struct RecorderEventStream {
    receiver: Receiver<Event>,
    stop: Sender<Cmd>,
    worker: Option<JoinHandle<()>>,
}

impl RecorderEventStream {
    pub fn recv(&self) -> Result<Event, RecvError> {
        self.receiver.recv()
    }

    pub fn try_recv(&self) -> Result<Event, TryRecvError> {
        self.receiver.try_recv()
    }
}

impl Iterator for RecorderEventStream {
    type Item = Event;

    fn next(&mut self) -> Option<Self::Item> {
        self.recv().ok()
    }
}

impl Drop for RecorderEventStream {
    fn drop(&mut self) {
        let _ = self.stop.send(Cmd::Stop { announce: false });
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn validate_options(options: &ServiceOptions) -> Result<(), String> {
    if options.fps == 0 {
        return Err("recorder frame rate must be greater than zero".into());
    }
    if options.bitrate_bps == 0 {
        return Err("recorder bitrate must be greater than zero".into());
    }
    if !options.replay_window_s.is_finite() || options.replay_window_s <= 0.0 {
        return Err("recorder replay window must be finite and greater than zero".into());
    }
    if options.buffer_bytes == 0 {
        return Err("recorder buffer must be greater than zero".into());
    }
    if options.media_dir.as_os_str().is_empty() {
        return Err("recorder media directory cannot be empty".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::{MutexGuard, OnceLock};
    use std::time::Duration;

    fn test_guard() -> MutexGuard<'static, ()> {
        static TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn prepared_worker_stays_parked_and_drop_joins_without_running() {
        let _guard = test_guard();
        let ran = Arc::new(AtomicBool::new(false));
        let worker_ran = ran.clone();
        let before = active_recorder_workers();
        let prepared = PreparedRecorderRestart::prepare_with(move |_, _| {
            worker_ran.store(true, Ordering::Release);
        })
        .unwrap();
        assert_eq!(active_recorder_workers(), before + 1);
        assert!(matches!(
            prepared.events.as_ref().unwrap().try_recv(),
            Err(TryRecvError::Empty)
        ));
        drop(prepared);
        assert_eq!(active_recorder_workers(), before);
        assert!(!ran.load(Ordering::Acquire));
    }

    #[test]
    fn commit_starts_once_and_event_stream_joins_the_worker() {
        let _guard = test_guard();
        let (finished_tx, finished_rx) = mpsc::channel();
        let before = active_recorder_workers();
        let prepared = PreparedRecorderRestart::prepare_with(move |commands, events| {
            let _ = events.send(Event::MediaRootResolved {
                path: "started".into(),
                fell_back: false,
            });
            let _ = commands.recv_timeout(Duration::from_secs(1));
            let _ = finished_tx.send(());
        })
        .unwrap();
        let stream = prepared.commit();
        assert!(matches!(
            stream.recv().unwrap(),
            Event::MediaRootResolved { path, fell_back: false } if path == "started"
        ));
        drop(stream);
        finished_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(active_recorder_workers(), before);
    }

    #[test]
    fn worker_panic_becomes_terminal_events_and_remains_joinable() {
        let _guard = test_guard();
        let prepared = PreparedRecorderRestart::prepare_with(|_, _| panic!("boom")).unwrap();
        let stream = prepared.commit();
        assert!(matches!(
            stream.recv().unwrap(),
            Event::Error { message } if message == "recorder worker panicked"
        ));
        assert!(matches!(
            stream.recv().unwrap(),
            Event::Status {
                recording: false,
                waiting_for_game: false,
                ..
            }
        ));
        drop(stream);
    }

    #[test]
    fn commit_can_publish_latest_validated_runtime_context_before_start() {
        let _guard = test_guard();
        let initial = ServiceOptions::default();
        let slot = Arc::new(Mutex::new(initial.clone()));
        let worker_slot = slot.clone();
        let mut prepared = PreparedRecorderRestart::prepare_with(move |_, events| {
            let fps = worker_slot
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .fps;
            let _ = events.send(Event::MediaRootResolved {
                path: fps.to_string(),
                fell_back: false,
            });
        })
        .unwrap();
        prepared.options = Some(slot);
        let latest = ServiceOptions {
            fps: 120,
            ..initial
        };

        let stream = prepared.commit_with_options(latest);

        assert!(matches!(
            stream.recv().unwrap(),
            Event::MediaRootResolved { path, fell_back: false } if path == "120"
        ));
        drop(stream);
    }
}
