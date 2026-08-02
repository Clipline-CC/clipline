use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use clipline_playback::windows::{
    session_channel, D3D11PublisherTelemetry, D3D11VideoSurface, SessionClient, SessionExit,
    SessionTelemetry, SessionUpdatePayload, WindowsD3D11Publisher,
};
use clipline_playback::{
    BackendError, DecodedVideoFrame, FramePublisher, PipelineToken, PlaybackCommand, PlaybackEvent,
    PlaybackTime, PublicationReceipt, PLAYBACK_TIMELINE_HZ,
};

use crate::controller::{
    ApplyUpdateOutcome, ControllerUpdate, ControllerUpdatePayload, PlaybackCommandPort,
    PlaybackController, UiPlaybackState,
};
use crate::cpu_frame::{CpuDiagnosticPublisher, CpuFrameTelemetry};
use crate::model::format_clock;
use crate::options::{write_marker, SpikeScenario};
use crate::CliplineSpike;

const UPDATE_POLL: Duration = Duration::from_millis(2);
const SCRUB_INTERVAL: Duration = Duration::from_millis(75);

#[derive(Clone)]
pub struct SessionCommandPort {
    client: Arc<SessionClient>,
}

impl PlaybackCommandPort for SessionCommandPort {
    fn send(&self, command: PlaybackCommand) -> Result<(), String> {
        self.client
            .try_send(command)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
}

pub enum SpikePublisher {
    D3d(WindowsD3D11Publisher),
    Cpu(CpuDiagnosticPublisher),
}

impl FramePublisher<D3D11VideoSurface> for SpikePublisher {
    fn publish(
        &mut self,
        frame: DecodedVideoFrame<D3D11VideoSurface>,
    ) -> Result<PublicationReceipt, BackendError> {
        match self {
            Self::D3d(publisher) => publisher.publish(frame),
            Self::Cpu(publisher) => publisher.publish(frame),
        }
    }

    fn clear(&mut self, token: PipelineToken) -> Result<(), BackendError> {
        match self {
            Self::D3d(publisher) => publisher.clear(token),
            Self::Cpu(publisher) => publisher.clear(token),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresentationTelemetry {
    D3d(D3D11PublisherTelemetry),
    Cpu {
        frames: CpuFrameTelemetry,
        readback_frames: u64,
        max_copy_time_100ns: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveSessionReport {
    pub exit: SessionExit,
    pub telemetry: Option<SessionTelemetry>,
    pub presentation: PresentationTelemetry,
}

pub struct LiveSession {
    controller: Arc<Mutex<PlaybackController<SessionCommandPort>>>,
    stop_updates: Arc<AtomicBool>,
    playback: Option<
        JoinHandle<
            Result<
                clipline_playback::windows::SessionReport<SpikePublisher>,
                clipline_playback::windows::SessionRunError,
            >,
        >,
    >,
    updates: Option<JoinHandle<()>>,
}

impl LiveSession {
    pub fn start(
        publisher: SpikePublisher,
        window: slint::Weak<CliplineSpike>,
        fixture: PathBuf,
        scenario: SpikeScenario,
        marker_path: Option<PathBuf>,
        stop_path: Option<PathBuf>,
        exit_after_ready: bool,
    ) -> Result<Self, String> {
        let (client, runtime) = session_channel();
        let client = Arc::new(client);
        let port = SessionCommandPort {
            client: Arc::clone(&client),
        };
        let controller = Arc::new(Mutex::new(PlaybackController::new(port)));
        let playback = thread::Builder::new()
            .name("clipline-slint-playback".to_owned())
            .spawn(move || runtime.run(publisher))
            .map_err(|error| format!("spawn native playback thread: {error}"))?;
        client
            .try_send(PlaybackCommand::Open { path: fixture })
            .map_err(|error| format!("open native playback fixture: {error}"))?;
        let stop_updates = Arc::new(AtomicBool::new(false));
        let updates = spawn_update_pump(
            Arc::clone(&client),
            Arc::clone(&controller),
            UpdatePumpConfig {
                stop: Arc::clone(&stop_updates),
                window,
                scenario,
                marker_path,
                stop_path,
                exit_after_ready,
            },
        )?;
        Ok(Self {
            controller,
            stop_updates,
            playback: Some(playback),
            updates: Some(updates),
        })
    }

    pub fn controller(&self) -> Arc<Mutex<PlaybackController<SessionCommandPort>>> {
        Arc::clone(&self.controller)
    }

    pub fn shutdown(mut self) -> Result<LiveSessionReport, String> {
        self.stop_updates.store(true, Ordering::Release);
        let updates_result = self.updates.take().map(JoinHandle::join);
        if let Ok(controller) = self.controller.lock() {
            let _ = controller.close();
        }
        let playback = self
            .playback
            .take()
            .ok_or_else(|| "native playback thread was already joined".to_owned())?;
        let playback_result = playback.join();
        if let Some(updates_result) = updates_result {
            updates_result.map_err(|_| "Slint update pump panicked".to_owned())?;
        }
        let mut report = playback_result
            .map_err(|_| "native playback thread panicked".to_owned())?
            .map_err(|error| format!("native playback session failed: {error}"))?;
        let presentation = match &mut report.publisher {
            SpikePublisher::D3d(publisher) => {
                let telemetry = publisher.telemetry();
                publisher.close();
                PresentationTelemetry::D3d(telemetry)
            }
            SpikePublisher::Cpu(publisher) => {
                let readback = publisher.readback_telemetry();
                PresentationTelemetry::Cpu {
                    frames: publisher.frame_telemetry(),
                    readback_frames: readback.frames_read,
                    max_copy_time_100ns: readback.max_copy_time_100ns,
                }
            }
        };
        Ok(LiveSessionReport {
            exit: report.exit,
            telemetry: report.telemetry,
            presentation,
        })
    }
}

struct UpdatePumpConfig {
    stop: Arc<AtomicBool>,
    window: slint::Weak<CliplineSpike>,
    scenario: SpikeScenario,
    marker_path: Option<PathBuf>,
    stop_path: Option<PathBuf>,
    exit_after_ready: bool,
}

fn spawn_update_pump(
    client: Arc<SessionClient>,
    controller: Arc<Mutex<PlaybackController<SessionCommandPort>>>,
    config: UpdatePumpConfig,
) -> Result<JoinHandle<()>, String> {
    let UpdatePumpConfig {
        stop,
        window,
        scenario,
        marker_path,
        stop_path,
        exit_after_ready,
    } = config;
    thread::Builder::new()
        .name("clipline-slint-updates".to_owned())
        .spawn(move || {
            let latest_ui_sequence = Arc::new(AtomicU64::new(0));
            let mut opened = false;
            let mut playing_requested = false;
            let mut settled_seeks = 0_u64;
            let mut next_scrub = Instant::now();
            let mut scrub_near_end = false;
            let mut marker_written = false;
            while !stop.load(Ordering::Acquire) {
                if stop_path.as_ref().is_some_and(|path| path.exists()) {
                    let _ = slint::quit_event_loop();
                }
                let mut progressed = false;
                while let Some(update) = client.try_recv_update() {
                    progressed = true;
                    let sequence = update.sequence;
                    let event = match &update.payload {
                        SessionUpdatePayload::Event(event) => Some(event.clone()),
                        _ => None,
                    };
                    let payload = match update.payload {
                        SessionUpdatePayload::Snapshot(snapshot) => {
                            ControllerUpdatePayload::Snapshot(snapshot)
                        }
                        SessionUpdatePayload::Event(event) => ControllerUpdatePayload::Event(event),
                        SessionUpdatePayload::Metrics(_) => ControllerUpdatePayload::Metrics,
                    };
                    let state = if let Ok(mut controller) = controller.lock() {
                        (controller.apply_update(ControllerUpdate {
                            sequence,
                            token: update.token,
                            payload,
                        }) == ApplyUpdateOutcome::Applied)
                            .then(|| controller.ui_state().clone())
                    } else {
                        None
                    };
                    if let Some(state) = state {
                        latest_ui_sequence.store(sequence, Ordering::Release);
                        let gate = Arc::clone(&latest_ui_sequence);
                        let weak = window.clone();
                        if slint::invoke_from_event_loop(move || {
                            if gate.load(Ordering::Acquire) == sequence {
                                if let Some(window) = weak.upgrade() {
                                    apply_ui_state(&window, &state);
                                }
                            }
                        })
                        .is_err()
                        {
                            stop.store(true, Ordering::Release);
                            break;
                        }
                    }
                    if let Some(event) = event {
                        match event {
                            PlaybackEvent::Opened { .. } => opened = true,
                            PlaybackEvent::SeekSettled { .. } => {
                                settled_seeks = settled_seeks.saturating_add(1);
                            }
                            PlaybackEvent::Error { message, .. } => {
                                if let Some(path) = marker_path.as_ref() {
                                    let _ = write_marker(path, "error", &message);
                                }
                                marker_written = true;
                                if exit_after_ready {
                                    let _ = slint::quit_event_loop();
                                }
                            }
                            _ => {}
                        }
                    }
                }

                if opened
                    && !playing_requested
                    && scenario == SpikeScenario::ReviewPlaying
                    && client.try_send(PlaybackCommand::Play).is_ok()
                {
                    playing_requested = true;
                }
                if opened && scenario == SpikeScenario::ScrubStorm && Instant::now() >= next_scrub {
                    let ticks = if scrub_near_end {
                        4 * u64::from(PLAYBACK_TIMELINE_HZ)
                    } else {
                        u64::from(PLAYBACK_TIMELINE_HZ) / 2
                    };
                    let _ = client.try_send(PlaybackCommand::Seek {
                        position: PlaybackTime {
                            ticks,
                            timescale: PLAYBACK_TIMELINE_HZ,
                        },
                    });
                    scrub_near_end = !scrub_near_end;
                    next_scrub = Instant::now() + SCRUB_INTERVAL;
                }

                if !marker_written {
                    let ready = match scenario {
                        SpikeScenario::Interactive | SpikeScenario::ReviewIdle => opened,
                        SpikeScenario::ReviewPlaying => {
                            controller.lock().ok().is_some_and(|controller| {
                                controller.ui_state().playing
                                    && controller.ui_state().position.ticks > 0
                            })
                        }
                        SpikeScenario::ScrubStorm => settled_seeks >= 10,
                        SpikeScenario::RevealClose100 => false,
                    };
                    if ready {
                        if let Some(path) = marker_path.as_ref() {
                            let detail = format!("{} native Slint state ready", scenario.label());
                            let _ = write_marker(path, "ready", &detail);
                        }
                        marker_written = true;
                        if exit_after_ready {
                            let _ = slint::quit_event_loop();
                        }
                    }
                }
                if !progressed {
                    thread::sleep(UPDATE_POLL);
                }
            }
        })
        .map_err(|error| format!("spawn Slint update pump: {error}"))
}

fn apply_ui_state(window: &CliplineSpike, state: &UiPlaybackState) {
    window.set_playing(state.playing);
    window.set_volume(state.volume);
    window.set_current_time(format_clock(to_timeline_ticks(state.position)).into());
    window.set_duration_time(
        state
            .duration
            .map(to_timeline_ticks)
            .map(format_clock)
            .unwrap_or_else(|| "--:--.---".to_owned())
            .into(),
    );
    if state.audio_track_indices.len() >= 2 {
        window.set_track_one_id(i32::try_from(state.audio_track_indices[0]).unwrap_or(i32::MAX));
        window.set_track_two_id(i32::try_from(state.audio_track_indices[1]).unwrap_or(i32::MAX));
    }
    let track_one = usize::try_from(window.get_track_one_id()).ok();
    let track_two = usize::try_from(window.get_track_two_id()).ok();
    window.set_track_one_selected(
        track_one.is_some_and(|track| state.audio_track_indices.contains(&track)),
    );
    window.set_track_two_selected(
        track_two.is_some_and(|track| state.audio_track_indices.contains(&track)),
    );
    window.set_status_text(state.status.clone().into());
}

fn to_timeline_ticks(time: PlaybackTime) -> u64 {
    let scaled = u128::from(time.ticks).saturating_mul(u128::from(PLAYBACK_TIMELINE_HZ));
    u64::try_from(scaled / u128::from(time.timescale)).unwrap_or(u64::MAX)
}
