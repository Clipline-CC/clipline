use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::Mutex;
use std::time::Duration;

use tauri::{
    AppHandle, Emitter, Manager, Runtime,
};


use super::*;

#[derive(serde::Serialize, Clone)]
// Tauri events are JSON, so the live monitor keeps 30 ms chunks as compact
// i16 samples instead of shipping f32 PCM through IPC.
pub(crate) struct MicMonitorEvent {
    pub(crate) rms: f32,
    pub(crate) peak: f32,
    pub(crate) sample_count: usize,
    pub(crate) samples: Vec<i16>,
}

#[derive(Default)]
pub(crate) struct NativeMediaFolderAuthorization(pub(crate) Mutex<Option<PathBuf>>);

impl NativeMediaFolderAuthorization {
    pub(crate) fn authorize(&self, path: PathBuf) {
        if let Ok(mut pending) = self.0.lock() {
            *pending = Some(path);
        }
    }

    pub(crate) fn validate_change(&self, current: &Path, requested: &Path) -> Result<(), String> {
        if same_path(current, requested) {
            return Ok(());
        }
        let pending = self
            .0
            .lock()
            .map_err(|_| "native media-folder authorization is unavailable".to_string())?;
        if pending
            .as_deref()
            .is_some_and(|authorized| same_path(authorized, requested))
        {
            Ok(())
        } else {
            Err("choose a new media folder with the native folder picker first".into())
        }
    }

    pub(crate) fn commit(&self, path: &Path) {
        if let Ok(mut pending) = self.0.lock() {
            if pending
                .as_deref()
                .is_some_and(|authorized| same_path(authorized, path))
            {
                *pending = None;
            }
        }
    }
}

pub(crate) fn same_path(left: &Path, right: &Path) -> bool {
    crate::settings::validation::same_or_nested_path(left, right)
        && crate::settings::validation::same_or_nested_path(right, left)
}

pub(crate) fn display_media_folder_path(path: &Path) -> String {
    let path = path.to_string_lossy();
    let lowercase = path.to_ascii_lowercase();
    if lowercase.starts_with(r"\\?\unc\") {
        format!(r"\\{}", &path[8..])
    } else if lowercase.starts_with(r"\\?\") {
        path[4..].to_string()
    } else {
        path.into_owned()
    }
}

#[derive(Default)]
pub(crate) struct MicTestState(pub(crate) Mutex<MicTestInner>);

#[derive(Default)]
pub(crate) struct MicTestInner {
    pub(crate) last_generation: u64,
    pub(crate) active: Option<MicTestSession>,
}

pub(crate) struct MicTestSession {
    pub(crate) generation: u64,
    pub(crate) stop: Sender<()>,
}

impl MicTestState {
    pub(crate) fn begin(&self) -> Result<(u64, Receiver<()>), String> {
        let (stop, receiver) = mpsc::channel();
        let mut inner = self
            .0
            .lock()
            .map_err(|_| "mic test state lock poisoned".to_string())?;
        inner.last_generation = inner.last_generation.wrapping_add(1).max(1);
        let generation = inner.last_generation;
        let previous = inner.active.replace(MicTestSession { generation, stop });
        if let Some(previous) = previous {
            // Sending is non-blocking for this unbounded control channel. Keep
            // replacement and stop notification in one critical section so a
            // concurrent start cannot create an untracked interval.
            let _ = previous.stop.send(());
        }
        Ok((generation, receiver))
    }

    #[cfg(test)]
    pub(crate) fn is_active(&self, generation: u64) -> bool {
        self.0
            .lock()
            .map(|inner| {
                inner
                    .active
                    .as_ref()
                    .is_some_and(|active| active.generation == generation)
            })
            .unwrap_or(false)
    }

    /// Run a publication while this generation still owns the session lock.
    /// Replacement cannot install a newer generation between the ownership
    /// check and the event, which keeps event order authoritative for the UI.
    pub(crate) fn publish_if_active(&self, generation: u64, publish: impl FnOnce()) -> bool {
        let Ok(inner) = self.0.lock() else {
            return false;
        };
        if inner
            .active
            .as_ref()
            .is_none_or(|active| active.generation != generation)
        {
            return false;
        }
        publish();
        true
    }

    pub(crate) fn finish_if_active_with(&self, generation: u64, finish: impl FnOnce()) -> bool {
        let Ok(mut inner) = self.0.lock() else {
            return false;
        };
        if inner
            .active
            .as_ref()
            .is_none_or(|active| active.generation != generation)
        {
            return false;
        }
        inner.active.take();
        finish();
        true
    }

    pub(crate) fn finish_if_active(&self, generation: u64) -> bool {
        self.finish_if_active_with(generation, || {})
    }

    pub(crate) fn stop(&self) {
        match self.0.lock() {
            Ok(mut inner) => {
                if let Some(session) = inner.active.take() {
                    // Receiver gone means the test thread already exited — not an error.
                    let _ = session.stop.send(());
                }
            }
            Err(e) => tracing::error!(event = "mic_test_state_lock_poisoned", error = %e),
        }
    }
}

pub(crate) fn mic_test_should_stop(receiver: &Receiver<()>) -> bool {
    match receiver.try_recv() {
        Ok(()) | Err(TryRecvError::Disconnected) => true,
        Err(TryRecvError::Empty) => false,
    }
}

#[tauri::command]
pub(crate) fn start_microphone_test<R: Runtime>(
    app: AppHandle<R>,
    state: tauri::State<MicTestState>,
    window_lifecycle: tauri::State<WindowLifecycleState>,
    device_id: Option<String>,
    volume: f64,
    mono: bool,
) -> Result<(), String> {
    ensure_foreground_microphone_test(&window_lifecycle)?;
    let channels = if mono {
        clipline_capture::windows::wasapi::WasapiChannelMode::Mono
    } else {
        clipline_capture::windows::wasapi::WasapiChannelMode::Stereo
    };
    let (generation, stop_rx) = state.begin()?;
    if let Err(error) = ensure_foreground_microphone_test(&window_lifecycle) {
        state.finish_if_active(generation);
        return Err(error);
    }
    let worker_app = app.clone();
    let worker = std::thread::Builder::new()
        .name(format!("clipline-mic-test-{generation}"))
        .spawn(move || {
            let run = || -> Result<(), String> {
                let clock = clipline_capture::clock::RelativeClock::new(
                    clipline_capture::windows::qpc_now_ticks_100ns().map_err(|e| e.to_string())?,
                );
                let mut source =
                    clipline_capture::windows::wasapi::WasapiLoopback::start_microphone(
                        clock,
                        device_id.as_deref(),
                        volume,
                        channels,
                    )
                    .map_err(|e| e.to_string())?;
                loop {
                    if mic_test_should_stop(&stop_rx) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(30));
                    if mic_test_should_stop(&stop_rx) {
                        break;
                    }
                    let chunk = source.poll_monitor_chunk().map_err(|e| e.to_string())?;
                    let samples = chunk
                        .samples
                        .into_iter()
                        .map(|sample| {
                            let scaled = (sample.clamp(-1.0, 1.0) * 32_768.0).round();
                            scaled.clamp(i16::MIN as f32, i16::MAX as f32) as i16
                        })
                        .collect();
                    let mic_state = worker_app.state::<MicTestState>();
                    mic_state.publish_if_active(generation, || {
                        let _ = worker_app.emit(
                            "mic-test",
                            MicMonitorEvent {
                                rms: chunk.level.rms,
                                peak: chunk.level.peak,
                                sample_count: chunk.level.sample_count,
                                samples,
                            },
                        );
                    });
                }
                Ok(())
            };
            if let Err(e) = run() {
                let mic_state = worker_app.state::<MicTestState>();
                mic_state.finish_if_active_with(generation, || {
                    let _ = worker_app.emit("mic-test-error", e);
                    let _ = worker_app.emit("mic-test-stopped", ());
                });
            }
        });
    if let Err(error) = worker {
        state.finish_if_active(generation);
        return Err(format!("could not start microphone test thread: {error}"));
    }
    Ok(())
}

#[tauri::command]
pub(crate) fn stop_microphone_test(state: tauri::State<MicTestState>) {
    state.stop();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{WindowLifecycleMode, WindowLifecycleState, ensure_foreground_microphone_test};
    use std::sync::atomic::Ordering;

    #[test]
    fn microphone_test_stop_channel_treats_disconnect_as_shutdown() {
        let (sender, receiver) = mpsc::channel();
        assert!(!mic_test_should_stop(&receiver));
        sender.send(()).unwrap();
        assert!(mic_test_should_stop(&receiver));

        let (sender, receiver) = mpsc::channel();
        drop(sender);
        assert!(mic_test_should_stop(&receiver));
    }

    #[test]
    fn concurrent_microphone_test_starts_leave_one_tracked_generation() {
        const STARTS: usize = 12;
        let state = std::sync::Arc::new(MicTestState::default());
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(STARTS));
        let workers = (0..STARTS)
            .map(|_| {
                let state = state.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    state.begin().expect("session replacement")
                })
            })
            .collect::<Vec<_>>();
        let sessions = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>();

        let active = sessions
            .iter()
            .filter(|(generation, _)| state.is_active(*generation))
            .count();
        assert_eq!(active, 1);
        for (generation, receiver) in &sessions {
            assert_eq!(
                mic_test_should_stop(receiver),
                !state.is_active(*generation),
                "every superseded receiver must observe shutdown"
            );
        }
    }

    #[test]
    fn stale_microphone_test_cannot_publish_or_finish_active_generation() {
        let state = MicTestState::default();
        let (old_generation, _old_receiver) = state.begin().unwrap();
        let (active_generation, _active_receiver) = state.begin().unwrap();
        let published = std::sync::atomic::AtomicUsize::new(0);

        assert!(!state.publish_if_active(old_generation, || {
            published.fetch_add(1, Ordering::Relaxed);
        }));
        assert!(state.publish_if_active(active_generation, || {
            published.fetch_add(1, Ordering::Relaxed);
        }));
        assert_eq!(published.load(Ordering::Relaxed), 1);

        assert!(!state.finish_if_active(old_generation));
        assert!(state.is_active(active_generation));
        assert!(state.finish_if_active(active_generation));
        assert!(!state.is_active(active_generation));
    }

    #[test]
    fn microphone_test_rejects_destroying_and_destroyed_modes() {
        let state = WindowLifecycleState::default();
        state.transition(WindowLifecycleMode::Destroying);
        assert!(ensure_foreground_microphone_test(&state).is_err());
        state.transition(WindowLifecycleMode::Destroyed);
        assert!(ensure_foreground_microphone_test(&state).is_err());
    }

    #[test]
    fn microphone_test_requires_foreground_window_lifecycle() {
        let state = WindowLifecycleState::default();
        assert!(ensure_foreground_microphone_test(&state).is_err());

        state.transition(WindowLifecycleMode::Foreground);
        assert!(ensure_foreground_microphone_test(&state).is_ok());

        state.transition(WindowLifecycleMode::Taskbar);
        assert!(ensure_foreground_microphone_test(&state).is_err());
    }

    #[test]
    fn native_media_folder_authorization_is_exact_retryable_and_consumed_on_commit() {
        let authorization = NativeMediaFolderAuthorization::default();
        let old = PathBuf::from(r"C:\Users\tester\Videos\Clipline");
        let selected = PathBuf::from(r"D:\Recordings\Clipline");
        let other = PathBuf::from(r"D:\Other");

        assert!(authorization.validate_change(&old, &old).is_ok());
        assert!(authorization.validate_change(&old, &selected).is_err());

        authorization.authorize(selected.clone());
        assert!(authorization.validate_change(&old, &selected).is_ok());
        assert!(authorization.validate_change(&old, &selected).is_ok());
        assert!(authorization.validate_change(&old, &other).is_err());

        authorization.commit(&selected);
        assert!(authorization.validate_change(&old, &selected).is_err());
        assert!(authorization.validate_change(&selected, &selected).is_ok());
    }

    #[test]
    fn media_folder_display_path_removes_windows_verbatim_prefixes() {
        assert_eq!(
            display_media_folder_path(Path::new(r"\\?\C:\Users\tester\Videos\Clipline")),
            r"C:\Users\tester\Videos\Clipline"
        );
        assert_eq!(
            display_media_folder_path(Path::new(r"\\?\UNC\nas\clips")),
            r"\\nas\clips"
        );
        assert_eq!(
            display_media_folder_path(Path::new(r"D:\Clips")),
            r"D:\Clips"
        );
    }
}
