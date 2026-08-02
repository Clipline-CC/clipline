use std::fmt;

use clipline_playback::{MAX_SELECTED_AUDIO_TRACKS, PLAYBACK_TIMELINE_HZ};

pub const VISIBLE_LIBRARY_ROWS: usize = 24;
pub const MAX_PRESENTATION_MARKERS: usize = 4_096;
const MAX_MARKER_LABEL_BYTES: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpikeView {
    Library,
    Review,
}

impl SpikeView {
    pub fn show_library(&mut self) {
        *self = Self::Library;
    }

    pub fn show_review(&mut self) {
        *self = Self::Review;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipKind {
    Replay,
    Session,
    Trim,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryRow {
    pub title: String,
    pub subtitle: String,
    pub duration_ticks: u64,
    pub kind: ClipKind,
    pub poster_seed: u8,
}

pub fn representative_library() -> Vec<LibraryRow> {
    const GAMES: [&str; 4] = ["osu!", "League of Legends", "VALORANT", "Desktop"];
    (0..VISIBLE_LIBRARY_ROWS)
        .map(|index| {
            let kind = match index % 3 {
                0 => ClipKind::Replay,
                1 => ClipKind::Session,
                _ => ClipKind::Trim,
            };
            LibraryRow {
                title: format!("Clip {:02}", index + 1),
                subtitle: format!(
                    "{} · Today, {:02}:{:02}",
                    GAMES[index % GAMES.len()],
                    20 + index / 6,
                    (index * 7) % 60
                ),
                duration_ticks: (18 + index as u64 * 3) * u64::from(PLAYBACK_TIMELINE_HZ),
                kind,
                poster_seed: index as u8,
            }
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkerCategory {
    Event,
    Bookmark,
    Kill,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Marker {
    pub position_ticks: u64,
    pub label: String,
    pub category: MarkerCategory,
}

impl Marker {
    pub fn new(
        position_ticks: u64,
        label: impl Into<String>,
        category: MarkerCategory,
    ) -> Result<Self, PresentationModelError> {
        let label = label.into();
        if label.is_empty() {
            return Err(PresentationModelError::EmptyMarkerLabel);
        }
        if label.len() > MAX_MARKER_LABEL_BYTES {
            return Err(PresentationModelError::MarkerLabelTooLong {
                bytes: label.len(),
                max: MAX_MARKER_LABEL_BYTES,
            });
        }
        Ok(Self {
            position_ticks,
            label,
            category,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReviewState {
    duration_ticks: u64,
    position_ticks: u64,
    playing: bool,
    volume: f32,
    selected_tracks: Vec<u32>,
    markers: Vec<Marker>,
}

impl ReviewState {
    pub fn new(duration_ticks: u64) -> Self {
        Self {
            duration_ticks,
            position_ticks: 0,
            playing: false,
            volume: 1.0,
            selected_tracks: Vec::new(),
            markers: Vec::new(),
        }
    }

    pub fn set_position(&mut self, position_ticks: u64) {
        self.position_ticks = position_ticks.min(self.duration_ticks);
    }

    pub const fn position_ticks(&self) -> u64 {
        self.position_ticks
    }

    pub fn set_playing(&mut self, playing: bool) {
        self.playing = playing;
    }

    pub const fn is_playing(&self) -> bool {
        self.playing
    }

    pub fn set_volume(&mut self, volume: f32) -> Result<(), PresentationModelError> {
        if !volume.is_finite() {
            return Err(PresentationModelError::NonFiniteVolume);
        }
        self.volume = volume.clamp(0.0, 1.0);
        Ok(())
    }

    pub const fn volume(&self) -> f32 {
        self.volume
    }

    pub fn set_track_selected(
        &mut self,
        track_id: u32,
        selected: bool,
    ) -> Result<(), PresentationModelError> {
        match self
            .selected_tracks
            .binary_search_by_key(&track_id, |candidate| *candidate)
        {
            Ok(index) if !selected => {
                self.selected_tracks.remove(index);
            }
            Ok(_) => {}
            Err(_) if !selected => {}
            Err(index) => {
                if self.selected_tracks.len() >= MAX_SELECTED_AUDIO_TRACKS {
                    return Err(PresentationModelError::TooManySelectedTracks {
                        max: MAX_SELECTED_AUDIO_TRACKS,
                    });
                }
                self.selected_tracks.insert(index, track_id);
            }
        }
        Ok(())
    }

    pub fn selected_tracks(&self) -> &[u32] {
        &self.selected_tracks
    }

    pub fn set_markers(&mut self, markers: Vec<Marker>) -> Result<(), PresentationModelError> {
        if markers.len() > MAX_PRESENTATION_MARKERS {
            return Err(PresentationModelError::TooManyMarkers {
                count: markers.len(),
                max: MAX_PRESENTATION_MARKERS,
            });
        }
        if let Some(marker) = markers
            .iter()
            .find(|marker| marker.position_ticks > self.duration_ticks)
        {
            return Err(PresentationModelError::MarkerBeyondDuration {
                position_ticks: marker.position_ticks,
                duration_ticks: self.duration_ticks,
            });
        }
        let mut indexed: Vec<_> = markers.into_iter().enumerate().collect();
        indexed.sort_by_key(|(index, marker)| (marker.position_ticks, *index));
        self.markers = indexed.into_iter().map(|(_, marker)| marker).collect();
        Ok(())
    }

    pub fn markers(&self) -> &[Marker] {
        &self.markers
    }

    pub const fn transport_label(&self) -> &'static str {
        if self.playing {
            "Pause"
        } else {
            "Play"
        }
    }

    pub const fn rate_label(&self) -> &'static str {
        "1x"
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PresentationModelError {
    EmptyMarkerLabel,
    MarkerLabelTooLong {
        bytes: usize,
        max: usize,
    },
    TooManyMarkers {
        count: usize,
        max: usize,
    },
    MarkerBeyondDuration {
        position_ticks: u64,
        duration_ticks: u64,
    },
    TooManySelectedTracks {
        max: usize,
    },
    NonFiniteVolume,
}

impl fmt::Display for PresentationModelError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for PresentationModelError {}

pub fn format_clock(ticks: u64) -> String {
    let total_millis = ticks.saturating_mul(1_000) / u64::from(PLAYBACK_TIMELINE_HZ);
    let millis = total_millis % 1_000;
    let total_seconds = total_millis / 1_000;
    let seconds = total_seconds % 60;
    let total_minutes = total_seconds / 60;
    let minutes = total_minutes % 60;
    let hours = total_minutes / 60;
    if hours == 0 {
        format!("{minutes:02}:{seconds:02}.{millis:03}")
    } else {
        format!("{hours}:{minutes:02}:{seconds:02}.{millis:03}")
    }
}
