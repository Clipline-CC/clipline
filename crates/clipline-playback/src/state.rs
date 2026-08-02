use std::collections::BTreeSet;
use std::path::PathBuf;

use clipline_mp4::PlaybackTime;
use thiserror::Error;

use crate::{audio::MAX_SELECTED_AUDIO_TRACKS, PlaybackCommand};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WorkGeneration {
    pub open: u64,
    pub seek: u64,
}

impl WorkGeneration {
    pub const INITIAL: Self = Self { open: 0, seek: 0 };

    pub const fn new(open: u64, seek: u64) -> Self {
        Self { open, seek }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackPhase {
    Closed,
    Opening,
    Paused,
    Playing,
    Seeking,
    Ended,
    Failed,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlaybackSnapshot {
    pub phase: PlaybackPhase,
    pub generation: WorkGeneration,
    pub path: Option<PathBuf>,
    pub position: PlaybackTime,
    pub duration: Option<PlaybackTime>,
    pub audio_track_indices: Vec<usize>,
    pub volume: f32,
    pub rate: f32,
    pub playing_intent: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PlaybackEvent {
    Snapshot(PlaybackSnapshot),
    Opened {
        generation: WorkGeneration,
        duration: PlaybackTime,
    },
    SeekSettled {
        generation: WorkGeneration,
        position: PlaybackTime,
    },
    Ended {
        generation: WorkGeneration,
    },
    Error {
        generation: WorkGeneration,
        message: String,
    },
    Closed {
        generation: WorkGeneration,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CommandError {
    #[error("playback path is empty")]
    EmptyPath,
    #[error("no media is open")]
    NoMedia,
    #[error("media is still opening")]
    MediaNotReady,
    #[error("media playback failed; open or close before sending another command")]
    MediaFailed,
    #[error("playback volume must be finite and between zero and one")]
    InvalidVolume,
    #[error("playback rate must be finite and positive")]
    InvalidRate,
    #[error("playback rate {milli_rate}/1000 is not supported yet")]
    UnsupportedRate { milli_rate: i32 },
    #[error("step count must be non-zero")]
    InvalidStep,
    #[error("playback time must have a non-zero timescale")]
    InvalidTime,
    #[error("audio track {track_index} was selected more than once")]
    DuplicateTrack { track_index: usize },
    #[error("selected {count} audio tracks, exceeding the limit of {limit}")]
    TooManyAudioTracks { count: usize, limit: usize },
    #[error("playback generation counter is exhausted")]
    GenerationExhausted,
}

#[derive(Debug, Clone)]
pub struct PlaybackState {
    phase: PlaybackPhase,
    generation: WorkGeneration,
    path: Option<PathBuf>,
    position: PlaybackTime,
    duration: Option<PlaybackTime>,
    audio_track_indices: Vec<usize>,
    volume: f32,
    rate: f32,
    playing_intent: bool,
}

impl Default for PlaybackState {
    fn default() -> Self {
        Self {
            phase: PlaybackPhase::Closed,
            generation: WorkGeneration::INITIAL,
            path: None,
            position: PlaybackTime {
                ticks: 0,
                timescale: 1,
            },
            duration: None,
            audio_track_indices: Vec::new(),
            volume: 1.0,
            rate: 1.0,
            playing_intent: false,
        }
    }
}

impl PlaybackState {
    pub fn snapshot(&self) -> PlaybackSnapshot {
        PlaybackSnapshot {
            phase: self.phase,
            generation: self.generation,
            path: self.path.clone(),
            position: self.position,
            duration: self.duration,
            audio_track_indices: self.audio_track_indices.clone(),
            volume: self.volume,
            rate: self.rate,
            playing_intent: self.playing_intent,
        }
    }

    pub fn generation(&self) -> WorkGeneration {
        self.generation
    }

    pub fn accepts(&self, generation: WorkGeneration) -> bool {
        self.generation == generation
    }

    pub fn apply(&mut self, command: PlaybackCommand) -> Result<WorkGeneration, CommandError> {
        if self.phase == PlaybackPhase::Failed
            && !matches!(
                &command,
                PlaybackCommand::Open { .. } | PlaybackCommand::Close
            )
        {
            return Err(CommandError::MediaFailed);
        }
        match command {
            PlaybackCommand::Open { path } => {
                if path.as_os_str().is_empty() {
                    return Err(CommandError::EmptyPath);
                }
                let next_open = self
                    .generation
                    .open
                    .checked_add(1)
                    .ok_or(CommandError::GenerationExhausted)?;
                self.generation = WorkGeneration::new(next_open, 0);
                self.phase = PlaybackPhase::Opening;
                self.path = Some(path);
                self.position = PlaybackTime {
                    ticks: 0,
                    timescale: 1,
                };
                self.duration = None;
                self.audio_track_indices.clear();
                self.playing_intent = false;
            }
            PlaybackCommand::Close => {
                let next_open = self
                    .generation
                    .open
                    .checked_add(1)
                    .ok_or(CommandError::GenerationExhausted)?;
                self.generation = WorkGeneration::new(next_open, 0);
                self.phase = PlaybackPhase::Closed;
                self.path = None;
                self.position = PlaybackTime {
                    ticks: 0,
                    timescale: 1,
                };
                self.duration = None;
                self.audio_track_indices.clear();
                self.playing_intent = false;
            }
            PlaybackCommand::Play => {
                self.require_media()?;
                self.playing_intent = true;
                if self.phase != PlaybackPhase::Opening && self.phase != PlaybackPhase::Seeking {
                    self.phase = PlaybackPhase::Playing;
                }
            }
            PlaybackCommand::Pause => {
                self.require_media()?;
                self.playing_intent = false;
                if self.phase != PlaybackPhase::Opening && self.phase != PlaybackPhase::Seeking {
                    self.phase = PlaybackPhase::Paused;
                }
            }
            PlaybackCommand::Seek { position } => {
                Self::validate_time(position)?;
                self.require_ready_media()?;
                self.begin_seek()?;
                self.position = position;
            }
            PlaybackCommand::Step { frames } => {
                self.require_ready_media()?;
                if frames == 0 {
                    return Err(CommandError::InvalidStep);
                }
                self.begin_seek()?;
                self.playing_intent = false;
            }
            PlaybackCommand::SetTracks {
                audio_track_indices,
            } => {
                self.require_ready_media()?;
                Self::validate_audio_tracks(&audio_track_indices)?;
                if self.audio_track_indices != audio_track_indices {
                    self.begin_seek()?;
                    self.audio_track_indices = audio_track_indices;
                }
            }
            PlaybackCommand::SetVolume { volume } => {
                if !volume.is_finite() || !(0.0..=1.0).contains(&volume) {
                    return Err(CommandError::InvalidVolume);
                }
                self.volume = volume;
            }
            PlaybackCommand::SetRate { rate } => {
                if !rate.is_finite() || rate <= 0.0 {
                    return Err(CommandError::InvalidRate);
                }
                if rate.to_bits() != 1.0_f32.to_bits() {
                    return Err(CommandError::UnsupportedRate {
                        milli_rate: (rate * 1_000.0).round() as i32,
                    });
                }
                self.rate = rate;
            }
        }
        Ok(self.generation)
    }

    pub(crate) fn install_open_audio_tracks(
        &mut self,
        generation: WorkGeneration,
        audio_track_indices: Vec<usize>,
    ) -> Result<bool, CommandError> {
        Self::validate_audio_tracks(&audio_track_indices)?;
        if !self.accepts(generation) || self.phase != PlaybackPhase::Opening {
            return Ok(false);
        }
        self.audio_track_indices = audio_track_indices;
        Ok(true)
    }

    pub fn complete_open(&mut self, generation: WorkGeneration, duration: PlaybackTime) -> bool {
        if !self.accepts(generation)
            || self.phase != PlaybackPhase::Opening
            || Self::validate_time(duration).is_err()
        {
            return false;
        }
        self.duration = Some(duration);
        self.phase = if self.playing_intent {
            PlaybackPhase::Playing
        } else {
            PlaybackPhase::Paused
        };
        true
    }

    pub fn complete_seek(&mut self, generation: WorkGeneration, position: PlaybackTime) -> bool {
        if !self.accepts(generation)
            || self.phase != PlaybackPhase::Seeking
            || Self::validate_time(position).is_err()
        {
            return false;
        }
        self.position = position;
        self.phase = if self.playing_intent {
            PlaybackPhase::Playing
        } else {
            PlaybackPhase::Paused
        };
        true
    }

    pub fn begin_recovery_seek(
        &mut self,
        generation: WorkGeneration,
        position: PlaybackTime,
    ) -> bool {
        if !self.accepts(generation)
            || self.require_ready_media().is_err()
            || Self::validate_time(position).is_err()
        {
            return false;
        }
        self.position = position;
        self.phase = PlaybackPhase::Seeking;
        true
    }

    pub fn update_position(&mut self, generation: WorkGeneration, position: PlaybackTime) -> bool {
        if !self.accepts(generation)
            || matches!(
                self.phase,
                PlaybackPhase::Closed
                    | PlaybackPhase::Opening
                    | PlaybackPhase::Ended
                    | PlaybackPhase::Failed
            )
            || Self::validate_time(position).is_err()
        {
            return false;
        }
        self.position = position;
        true
    }

    pub fn mark_ended(&mut self, generation: WorkGeneration, position: PlaybackTime) -> bool {
        if !self.update_position(generation, position) {
            return false;
        }
        self.phase = PlaybackPhase::Ended;
        self.playing_intent = false;
        true
    }

    pub fn fail(&mut self, generation: WorkGeneration) -> bool {
        if !self.accepts(generation) || self.phase == PlaybackPhase::Closed {
            return false;
        }
        self.phase = PlaybackPhase::Failed;
        self.playing_intent = false;
        true
    }

    fn require_media(&self) -> Result<(), CommandError> {
        if self.path.is_none() || self.phase == PlaybackPhase::Closed {
            Err(CommandError::NoMedia)
        } else {
            Ok(())
        }
    }

    fn require_ready_media(&self) -> Result<(), CommandError> {
        self.require_media()?;
        if self.phase == PlaybackPhase::Opening {
            Err(CommandError::MediaNotReady)
        } else {
            Ok(())
        }
    }

    fn begin_seek(&mut self) -> Result<(), CommandError> {
        let next_seek = self
            .generation
            .seek
            .checked_add(1)
            .ok_or(CommandError::GenerationExhausted)?;
        self.generation.seek = next_seek;
        self.phase = PlaybackPhase::Seeking;
        Ok(())
    }

    fn validate_audio_tracks(audio_track_indices: &[usize]) -> Result<(), CommandError> {
        if audio_track_indices.len() > MAX_SELECTED_AUDIO_TRACKS {
            return Err(CommandError::TooManyAudioTracks {
                count: audio_track_indices.len(),
                limit: MAX_SELECTED_AUDIO_TRACKS,
            });
        }
        let mut selected = BTreeSet::new();
        for &track_index in audio_track_indices {
            if !selected.insert(track_index) {
                return Err(CommandError::DuplicateTrack { track_index });
            }
        }
        Ok(())
    }

    fn validate_time(time: PlaybackTime) -> Result<(), CommandError> {
        if time.timescale == 0 {
            Err(CommandError::InvalidTime)
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready_state() -> PlaybackState {
        let mut state = PlaybackState::default();
        let generation = state
            .apply(PlaybackCommand::Open {
                path: PathBuf::from("clip.mp4"),
            })
            .unwrap();
        assert!(state.complete_open(
            generation,
            PlaybackTime {
                ticks: 10,
                timescale: 1,
            }
        ));
        state
    }

    #[test]
    fn exhausted_seek_generation_never_partially_mutates_state() {
        let mut step = ready_state();
        step.generation.seek = u64::MAX;
        step.playing_intent = true;
        let before = step.snapshot();
        assert_eq!(
            step.apply(PlaybackCommand::Step { frames: 1 }),
            Err(CommandError::GenerationExhausted)
        );
        assert_eq!(step.snapshot(), before);

        let mut tracks = ready_state();
        tracks.generation.seek = u64::MAX;
        let before = tracks.snapshot();
        assert_eq!(
            tracks.apply(PlaybackCommand::SetTracks {
                audio_track_indices: vec![1, 2],
            }),
            Err(CommandError::GenerationExhausted)
        );
        assert_eq!(tracks.snapshot(), before);
    }
}
