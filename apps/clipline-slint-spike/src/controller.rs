use std::fmt;
use std::path::PathBuf;

use clipline_playback::windows::{SessionUpdate, SessionUpdatePayload};
use clipline_playback::{
    PipelineToken, PlaybackCommand, PlaybackEvent, PlaybackPhase, PlaybackSnapshot, PlaybackTime,
    PLAYBACK_TIMELINE_HZ,
};

pub trait PlaybackCommandPort {
    fn send(&self, command: PlaybackCommand) -> Result<(), String>;
}

#[derive(Debug, Clone, PartialEq)]
pub struct UiPlaybackState {
    pub phase: PlaybackPhase,
    pub playing: bool,
    pub position: PlaybackTime,
    pub duration: Option<PlaybackTime>,
    pub audio_track_indices: Vec<usize>,
    pub volume: f32,
    pub status: String,
}

impl Default for UiPlaybackState {
    fn default() -> Self {
        Self {
            phase: PlaybackPhase::Closed,
            playing: false,
            position: PlaybackTime {
                ticks: 0,
                timescale: 1,
            },
            duration: None,
            audio_track_indices: Vec::new(),
            volume: 1.0,
            status: "Native playback idle".to_owned(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyUpdateOutcome {
    Applied,
    IgnoredStale,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControllerError {
    Command(String),
    NoSnapshot,
    InvalidSeek,
    ShutdownOrder {
        expected: ShutdownStage,
        actual: ShutdownStage,
    },
}

impl fmt::Display for ControllerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for ControllerError {}

pub struct PlaybackController<P> {
    port: P,
    latest_sequence: Option<u64>,
    latest_token: Option<PipelineToken>,
    snapshot: Option<PlaybackSnapshot>,
    ui: UiPlaybackState,
}

impl<P: PlaybackCommandPort> PlaybackController<P> {
    pub fn new(port: P) -> Self {
        Self {
            port,
            latest_sequence: None,
            latest_token: None,
            snapshot: None,
            ui: UiPlaybackState::default(),
        }
    }

    pub fn ui_state(&self) -> &UiPlaybackState {
        &self.ui
    }

    pub fn latest_token(&self) -> Option<PipelineToken> {
        self.latest_token
    }

    pub fn open(&self, path: PathBuf) -> Result<(), ControllerError> {
        self.send(PlaybackCommand::Open { path })
    }

    pub fn play_pause(&self) -> Result<(), ControllerError> {
        let snapshot = self.snapshot.as_ref().ok_or(ControllerError::NoSnapshot)?;
        let playing = snapshot.playing_intent || snapshot.phase == PlaybackPhase::Playing;
        self.send(if playing {
            PlaybackCommand::Pause
        } else {
            PlaybackCommand::Play
        })
    }

    pub fn seek_relative(&self, seconds: f64) -> Result<(), ControllerError> {
        if !seconds.is_finite() {
            return Err(ControllerError::InvalidSeek);
        }
        let snapshot = self.snapshot.as_ref().ok_or(ControllerError::NoSnapshot)?;
        let current = to_timeline_ticks(snapshot.position);
        let duration = snapshot.duration.map(to_timeline_ticks).unwrap_or(u64::MAX);
        let delta = seconds * f64::from(PLAYBACK_TIMELINE_HZ);
        if delta < i64::MIN as f64 || delta > i64::MAX as f64 {
            return Err(ControllerError::InvalidSeek);
        }
        let requested = if delta.is_sign_negative() {
            current.saturating_sub(delta.abs().round() as u64)
        } else {
            current.saturating_add(delta.round() as u64)
        }
        .min(duration);
        self.send(PlaybackCommand::Seek {
            position: PlaybackTime {
                ticks: requested,
                timescale: PLAYBACK_TIMELINE_HZ,
            },
        })
    }

    pub fn set_track(&self, track_index: usize, selected: bool) -> Result<(), ControllerError> {
        let snapshot = self.snapshot.as_ref().ok_or(ControllerError::NoSnapshot)?;
        let mut tracks = snapshot.audio_track_indices.clone();
        match tracks.binary_search(&track_index) {
            Ok(index) if !selected => {
                tracks.remove(index);
            }
            Ok(_) => {}
            Err(_) if !selected => {}
            Err(index) => tracks.insert(index, track_index),
        }
        self.send(PlaybackCommand::SetTracks {
            audio_track_indices: tracks,
        })
    }

    pub fn set_volume(&self, volume: f32) -> Result<(), ControllerError> {
        self.send(PlaybackCommand::SetVolume { volume })
    }

    pub fn close(&self) -> Result<(), ControllerError> {
        self.send(PlaybackCommand::Close)
    }

    pub fn apply_update(&mut self, update: SessionUpdate) -> ApplyUpdateOutcome {
        if self
            .latest_sequence
            .is_some_and(|sequence| update.sequence <= sequence)
            || self
                .latest_token
                .is_some_and(|token| token_is_older(update.token, token))
        {
            return ApplyUpdateOutcome::IgnoredStale;
        }
        self.latest_sequence = Some(update.sequence);
        self.latest_token = Some(update.token);
        match update.payload {
            SessionUpdatePayload::Snapshot(snapshot) => self.apply_snapshot(snapshot),
            SessionUpdatePayload::Event(event) => self.apply_event(event),
            SessionUpdatePayload::Metrics(_) => {}
        }
        ApplyUpdateOutcome::Applied
    }

    fn send(&self, command: PlaybackCommand) -> Result<(), ControllerError> {
        self.port.send(command).map_err(ControllerError::Command)
    }

    fn apply_snapshot(&mut self, snapshot: PlaybackSnapshot) {
        self.ui.phase = snapshot.phase;
        self.ui.playing = snapshot.phase == PlaybackPhase::Playing || snapshot.playing_intent;
        self.ui.position = snapshot.position;
        self.ui.duration = snapshot.duration;
        self.ui.audio_track_indices = snapshot.audio_track_indices.clone();
        self.ui.volume = snapshot.volume;
        self.ui.status = phase_status(snapshot.phase).to_owned();
        self.snapshot = Some(snapshot);
    }

    fn apply_event(&mut self, event: PlaybackEvent) {
        match event {
            PlaybackEvent::Error { message, .. } => {
                self.ui.phase = PlaybackPhase::Failed;
                self.ui.playing = false;
                self.ui.status = format!("Playback error: {message}");
            }
            PlaybackEvent::Opened { duration, .. } => {
                self.ui.duration = Some(duration);
                self.ui.status = "Native playback ready".to_owned();
            }
            PlaybackEvent::SeekSettled { position, .. } => {
                self.ui.position = position;
            }
            PlaybackEvent::Ended { .. } => {
                self.ui.phase = PlaybackPhase::Ended;
                self.ui.playing = false;
                self.ui.status = "Playback ended".to_owned();
            }
            PlaybackEvent::Closed { .. } => {
                self.ui = UiPlaybackState::default();
                self.snapshot = None;
            }
            PlaybackEvent::Snapshot(snapshot) => self.apply_snapshot(snapshot),
        }
    }
}

fn to_timeline_ticks(time: PlaybackTime) -> u64 {
    let scaled = u128::from(time.ticks).saturating_mul(u128::from(PLAYBACK_TIMELINE_HZ));
    u64::try_from(scaled / u128::from(time.timescale)).unwrap_or(u64::MAX)
}

fn token_is_older(candidate: PipelineToken, current: PipelineToken) -> bool {
    let candidate_work = candidate.work();
    let current_work = current.work();
    (
        candidate_work.open,
        candidate_work.seek,
        candidate.revision(),
    ) < (current_work.open, current_work.seek, current.revision())
}

const fn phase_status(phase: PlaybackPhase) -> &'static str {
    match phase {
        PlaybackPhase::Closed => "Native playback idle",
        PlaybackPhase::Opening => "Opening native media",
        PlaybackPhase::Paused => "Native playback paused",
        PlaybackPhase::Playing => "Native playback playing",
        PlaybackPhase::Seeking => "Seeking native media",
        PlaybackPhase::Ended => "Playback ended",
        PlaybackPhase::Failed => "Native playback failed",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownStage {
    Running,
    SessionStopped,
    HostDestroyed,
    UiDropped,
}

#[derive(Debug, Default)]
pub struct ShutdownOrder {
    stage: Option<ShutdownStage>,
}

impl ShutdownOrder {
    pub fn stage(&self) -> ShutdownStage {
        self.stage.unwrap_or(ShutdownStage::Running)
    }

    pub fn session_stopped(&mut self) -> Result<(), ControllerError> {
        self.advance(ShutdownStage::Running, ShutdownStage::SessionStopped)
    }

    pub fn host_destroyed(&mut self) -> Result<(), ControllerError> {
        self.advance(ShutdownStage::SessionStopped, ShutdownStage::HostDestroyed)
    }

    pub fn ui_dropped(&mut self) -> Result<(), ControllerError> {
        self.advance(ShutdownStage::HostDestroyed, ShutdownStage::UiDropped)
    }

    fn advance(
        &mut self,
        expected: ShutdownStage,
        next: ShutdownStage,
    ) -> Result<(), ControllerError> {
        let actual = self.stage();
        if actual != expected {
            return Err(ControllerError::ShutdownOrder { expected, actual });
        }
        self.stage = Some(next);
        Ok(())
    }
}
