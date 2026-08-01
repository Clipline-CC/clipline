//! Bounded, read-only sample index for finalized Clipline MP4 files.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::ops::Range;
use std::path::Path;

use crate::trim::{parse_movie_reader, TrimError};
use crate::{TrackConfig, VideoCodecParams};

#[derive(Debug)]
pub enum PlaybackIndexError {
    InvalidTime(String),
    InvalidTrack(String),
    InvalidSample(String),
    BufferTooSmall { required: usize, available: usize },
    Mp4(TrimError),
    Io(std::io::Error),
}

impl std::fmt::Display for PlaybackIndexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidTime(message) => write!(f, "invalid playback time: {message}"),
            Self::InvalidTrack(message) => write!(f, "invalid playback track: {message}"),
            Self::InvalidSample(message) => write!(f, "invalid playback sample: {message}"),
            Self::BufferTooSmall {
                required,
                available,
            } => write!(
                f,
                "sample requires a {required}-byte buffer, but only {available} bytes are available"
            ),
            Self::Mp4(error) => write!(f, "playback index: {error}"),
            Self::Io(error) => write!(f, "playback sample io: {error}"),
        }
    }
}

impl std::error::Error for PlaybackIndexError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Mp4(error) => Some(error),
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<TrimError> for PlaybackIndexError {
    fn from(value: TrimError) -> Self {
        Self::Mp4(value)
    }
}

impl From<std::io::Error> for PlaybackIndexError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaybackTime {
    pub ticks: u64,
    pub timescale: u32,
}

impl PlaybackTime {
    pub fn new(ticks: u64, timescale: u32) -> Result<Self, PlaybackIndexError> {
        if timescale == 0 {
            return Err(PlaybackIndexError::InvalidTime(
                "timescale must be non-zero".into(),
            ));
        }
        Ok(Self { ticks, timescale })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaybackTrackConfig {
    H264 {
        width: u16,
        height: u16,
        nal_length_size: u8,
        sps: Vec<Vec<u8>>,
        pps: Vec<Vec<u8>>,
    },
    Hevc {
        width: u16,
        height: u16,
        nal_length_size: u8,
        vps: Vec<Vec<u8>>,
        sps: Vec<Vec<u8>>,
        pps: Vec<Vec<u8>>,
    },
    Av1 {
        width: u16,
        height: u16,
        sequence_header_obu: Vec<u8>,
    },
    Opus {
        channels: u16,
        sample_rate: u32,
        pre_skip: u16,
    },
}

impl PlaybackTrackConfig {
    fn is_video(&self) -> bool {
        matches!(
            self,
            Self::H264 { .. } | Self::Hevc { .. } | Self::Av1 { .. }
        )
    }

    fn is_audio(&self) -> bool {
        matches!(self, Self::Opus { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SampleIndex {
    pub offset: u64,
    pub size: u32,
    pub dts: u64,
    pub pts: i64,
    pub duration: u32,
    pub is_sync: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackIndex {
    pub id: u32,
    pub timescale: u32,
    pub config: PlaybackTrackConfig,
    pub samples: Vec<SampleIndex>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MovieIndex {
    pub movie_timescale: u32,
    pub duration_ticks: u64,
    pub tracks: Vec<TrackIndex>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SampleCursor {
    pub track_index: usize,
    pub sample_index: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackSampleRange {
    pub track_index: usize,
    pub samples: Range<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeekPlan {
    pub requested_time: PlaybackTime,
    /// Requested time clamped to the video presentation interval and
    /// expressed in the video's integer timescale.
    pub target_time: PlaybackTime,
    pub video_sync_sample: SampleCursor,
    pub video_preroll: TrackSampleRange,
    pub audio_preroll: Vec<TrackSampleRange>,
}

pub struct IndexedMovie<R: Read + Seek> {
    reader: R,
    index: MovieIndex,
}

impl IndexedMovie<File> {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, PlaybackIndexError> {
        Self::from_reader(File::open(path)?)
    }
}

impl<R: Read + Seek> IndexedMovie<R> {
    pub fn from_reader(mut reader: R) -> Result<Self, PlaybackIndexError> {
        let parsed = parse_movie_reader(&mut reader)?;
        let mut tracks = Vec::with_capacity(parsed.tracks.len());
        for track in parsed.tracks {
            let config = playback_config(track.cfg, track.nal_length_size)?;
            let mut samples = Vec::with_capacity(track.samples.len());
            for sample in track.samples {
                samples.push(SampleIndex {
                    offset: u64::try_from(sample.offset).map_err(|_| {
                        PlaybackIndexError::InvalidSample("sample offset is too large".into())
                    })?,
                    size: sample.size,
                    dts: sample.decode_ticks,
                    pts: i64::try_from(sample.start_ticks).map_err(|_| {
                        PlaybackIndexError::InvalidSample(
                            "sample presentation timestamp exceeds i64".into(),
                        )
                    })?,
                    duration: sample.duration,
                    is_sync: sample.is_sync,
                });
            }
            tracks.push(TrackIndex {
                id: track.id,
                timescale: track.timescale,
                config,
                samples,
            });
        }
        Ok(Self {
            reader,
            index: MovieIndex {
                movie_timescale: parsed.movie_timescale,
                duration_ticks: parsed.duration_ticks,
                tracks,
            },
        })
    }

    pub fn index(&self) -> &MovieIndex {
        &self.index
    }

    pub fn into_reader(self) -> R {
        self.reader
    }

    pub fn seek_plan(
        &self,
        video_track_index: usize,
        audio_track_indices: &[usize],
        requested_time: PlaybackTime,
    ) -> Result<SeekPlan, PlaybackIndexError> {
        let video = self
            .index
            .tracks
            .get(video_track_index)
            .ok_or_else(|| invalid_track_index(video_track_index))?;
        if !video.config.is_video() {
            return Err(PlaybackIndexError::InvalidTrack(format!(
                "track {video_track_index} is not video"
            )));
        }
        let video_start = video
            .samples
            .first()
            .and_then(|sample| u64::try_from(sample.pts).ok())
            .ok_or_else(|| {
                PlaybackIndexError::InvalidTime(
                    "video track has no finite presentation interval".into(),
                )
            })?;
        let video_end = video
            .samples
            .last()
            .and_then(|sample| {
                u64::try_from(sample.pts)
                    .ok()?
                    .checked_add(u64::from(sample.duration))
            })
            .ok_or_else(|| {
                PlaybackIndexError::InvalidTime(
                    "video track has no finite presentation interval".into(),
                )
            })?;
        let target_time = PlaybackTime {
            ticks: rescale_time_floor(requested_time, video.timescale)
                .clamp(video_start, video_end),
            timescale: video.timescale,
        };

        let mut selected = BTreeSet::new();
        for &track_index in audio_track_indices {
            if !selected.insert(track_index) {
                return Err(PlaybackIndexError::InvalidTrack(format!(
                    "audio track {track_index} was selected more than once"
                )));
            }
            let track = self
                .index
                .tracks
                .get(track_index)
                .ok_or_else(|| invalid_track_index(track_index))?;
            if !track.config.is_audio() {
                return Err(PlaybackIndexError::InvalidTrack(format!(
                    "track {track_index} is not audio"
                )));
            }
        }

        let video_target_end = video
            .samples
            .partition_point(|sample| time_at_or_before(sample.pts, video.timescale, target_time));
        let sync_sample_index = video.samples[..video_target_end]
            .iter()
            .rposition(|sample| sample.is_sync)
            .ok_or_else(|| {
                PlaybackIndexError::InvalidTime(
                    "no video sync sample exists at or before the requested time".into(),
                )
            })?;
        let sync_sample = &video.samples[sync_sample_index];
        let video_preroll = TrackSampleRange {
            track_index: video_track_index,
            samples: sync_sample_index..video_target_end,
        };

        let mut audio_preroll = Vec::with_capacity(audio_track_indices.len());
        for &track_index in audio_track_indices {
            let track = &self.index.tracks[track_index];
            let start = track.samples.partition_point(|sample| {
                sample_end_at_or_before(sample, track.timescale, sync_sample.pts, video.timescale)
            });
            let end = track.samples.partition_point(|sample| {
                time_at_or_before(sample.pts, track.timescale, target_time)
            });
            audio_preroll.push(TrackSampleRange {
                track_index,
                samples: start.min(end)..end,
            });
        }

        Ok(SeekPlan {
            requested_time,
            target_time,
            video_sync_sample: SampleCursor {
                track_index: video_track_index,
                sample_index: sync_sample_index,
            },
            video_preroll,
            audio_preroll,
        })
    }

    pub fn read_sample_into(
        &mut self,
        track_index: usize,
        sample_index: usize,
        output: &mut [u8],
    ) -> Result<usize, PlaybackIndexError> {
        let sample = self
            .index
            .tracks
            .get(track_index)
            .ok_or_else(|| invalid_track_index(track_index))?
            .samples
            .get(sample_index)
            .ok_or_else(|| {
                PlaybackIndexError::InvalidSample(format!(
                    "track {track_index} has no sample {sample_index}"
                ))
            })?;
        let required = sample.size as usize;
        if output.len() < required {
            return Err(PlaybackIndexError::BufferTooSmall {
                required,
                available: output.len(),
            });
        }
        let offset = sample.offset;
        self.reader.seek(SeekFrom::Start(offset))?;
        self.reader.read_exact(&mut output[..required])?;
        Ok(required)
    }
}

fn playback_config(
    config: TrackConfig,
    nal_length_size: Option<u8>,
) -> Result<PlaybackTrackConfig, PlaybackIndexError> {
    let config = match config {
        TrackConfig::Video(video) => match video.codec {
            VideoCodecParams::H264 { sps, pps } => PlaybackTrackConfig::H264 {
                width: video.width,
                height: video.height,
                nal_length_size: require_nal_length_size(nal_length_size, "H.264")?,
                sps,
                pps,
            },
            VideoCodecParams::Hevc { vps, sps, pps } => PlaybackTrackConfig::Hevc {
                width: video.width,
                height: video.height,
                nal_length_size: require_nal_length_size(nal_length_size, "HEVC")?,
                vps,
                sps,
                pps,
            },
            VideoCodecParams::Av1 {
                sequence_header_obu,
            } => PlaybackTrackConfig::Av1 {
                width: video.width,
                height: video.height,
                sequence_header_obu,
            },
        },
        TrackConfig::Audio(audio) => PlaybackTrackConfig::Opus {
            channels: audio.channels,
            sample_rate: audio.sample_rate,
            pre_skip: audio.pre_skip,
        },
    };
    Ok(config)
}

fn require_nal_length_size(
    nal_length_size: Option<u8>,
    codec: &str,
) -> Result<u8, PlaybackIndexError> {
    nal_length_size.ok_or_else(|| {
        PlaybackIndexError::InvalidTrack(format!("{codec} track has no NAL length size"))
    })
}

fn invalid_track_index(track_index: usize) -> PlaybackIndexError {
    PlaybackIndexError::InvalidTrack(format!("movie has no track {track_index}"))
}

fn rescale_time_floor(time: PlaybackTime, target_timescale: u32) -> u64 {
    let scaled = u128::from(time.ticks) * u128::from(target_timescale) / u128::from(time.timescale);
    u64::try_from(scaled).unwrap_or(u64::MAX)
}

fn time_at_or_before(pts: i64, timescale: u32, target: PlaybackTime) -> bool {
    if pts < 0 {
        return true;
    }
    u128::from(pts as u64) * u128::from(target.timescale)
        <= u128::from(target.ticks) * u128::from(timescale)
}

fn sample_end_at_or_before(
    sample: &SampleIndex,
    sample_timescale: u32,
    boundary_pts: i64,
    boundary_timescale: u32,
) -> bool {
    if boundary_pts < 0 {
        return false;
    }
    let Some(sample_end) = sample.pts.checked_add(i64::from(sample.duration)) else {
        return false;
    };
    if sample_end < 0 {
        return true;
    }
    u128::from(sample_end as u64) * u128::from(boundary_timescale)
        <= u128::from(boundary_pts as u64) * u128::from(sample_timescale)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playback_time_rejects_zero_timescale() {
        assert!(PlaybackTime::new(0, 0).is_err());
    }

    #[test]
    fn time_comparisons_do_not_round_between_timescales() {
        let target = PlaybackTime::new(1, 3).unwrap();
        assert!(time_at_or_before(16_000, 48_000, target));
        assert!(!time_at_or_before(16_001, 48_000, target));
    }
}
