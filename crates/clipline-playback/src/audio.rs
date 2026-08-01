use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Seek};
use std::ops::Range;

use clipline_mp4::{IndexedMovie, MovieIndex, PlaybackTime, PlaybackTrackConfig, TrackSampleRange};
use shiguredo_opus::{packet_get_nb_samples, Decoder, DecoderConfig};
use thiserror::Error;

use crate::ring::{StereoRingBuffer, MAX_AUDIO_QUEUE_FRAMES};
use crate::WorkGeneration;

pub const MAX_OPUS_PACKET_BYTES: usize = 64 * 1024;
pub const MAX_OPUS_FRAME_SAMPLES: usize = 5_760;
pub const MAX_SELECTED_AUDIO_TRACKS: usize = 8;
const PLAYBACK_SAMPLE_RATE: u32 = 48_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioResetPoint {
    FileStart,
    MidStream,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OpusDecodeStats {
    pub decoded_packets: u64,
    pub decoded_frames: u64,
    pub pre_skip_frames: u64,
    pub corrupt_packets: u64,
    pub corrupt_frames: u64,
    pub resets: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TimelineAudioStats {
    pub mixed_frames: u64,
    pub silent_frames: u64,
    pub dropped_frames: u64,
    pub timeline_frames: u64,
    pub queue_high_water_frames: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MixOutcome {
    pub active_tracks: usize,
    pub frames: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioTrackSpec {
    pub track_index: usize,
    pub channels: u16,
    pub sample_rate: u32,
    pub pre_skip: u16,
}

impl AudioTrackSpec {
    pub fn new(
        track_index: usize,
        channels: u16,
        sample_rate: u32,
        pre_skip: u16,
    ) -> Result<Self, AudioError> {
        validate_opus_format(channels, sample_rate)?;
        Ok(Self {
            track_index,
            channels,
            sample_rate,
            pre_skip,
        })
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DecoderBankChange {
    pub kept: usize,
    pub added: usize,
    pub removed: usize,
}

#[derive(Debug, Error)]
pub enum AudioError {
    #[error("Opus playback supports one or two channels, not {channels}")]
    InvalidChannels { channels: u16 },
    #[error("Opus playback requires 48 kHz audio, not {sample_rate} Hz")]
    UnsupportedSampleRate { sample_rate: u32 },
    #[error("create or reset Opus decoder: {message}")]
    DecoderState { message: String },
    #[error("decode Opus packet: {message}")]
    Decode { message: String },
    #[error("Opus packet is {size} bytes, above the {limit}-byte cap")]
    PacketTooLarge { size: usize, limit: usize },
    #[error("could not reserve {requested} bytes for the bounded Opus packet buffer")]
    PacketBufferAllocationFailed { requested: usize },
    #[error("decoded Opus frame has {frames} frames, above the {limit}-frame cap")]
    DecodedFrameTooLarge { frames: usize, limit: usize },
    #[error("stale audio generation {actual:?}; active generation is {active:?}")]
    StaleGeneration {
        active: WorkGeneration,
        actual: WorkGeneration,
    },
    #[error("track frame has {actual} samples, expected {expected}")]
    FrameLengthMismatch { expected: usize, actual: usize },
    #[error("track input {track_input} contains a non-finite sample at index {sample_index}")]
    NonFiniteSample {
        track_input: usize,
        sample_index: usize,
    },
    #[error("mix output has {actual} samples, expected {expected}")]
    OutputLengthMismatch { expected: usize, actual: usize },
    #[error("audio frame has {samples} samples, which is not interleaved stereo")]
    NonStereoSampleCount { samples: usize },
    #[error("audio queue capacity {requested_frames} is invalid; maximum is {max_frames}")]
    InvalidQueueCapacity {
        requested_frames: usize,
        max_frames: usize,
    },
    #[error(
        "audio queue has {available_frames} frames available but {requested_frames} were requested"
    )]
    QueueFull {
        requested_frames: usize,
        available_frames: usize,
    },
    #[error("audio timeline moved backward from {cursor} to {requested}")]
    TimelineRegression { cursor: u64, requested: u64 },
    #[error("audio timeline arithmetic overflow")]
    TimelineOverflow,
    #[error("audio mix chunk has {frames} frames, above the {limit}-frame cap")]
    MixChunkTooLarge { frames: usize, limit: usize },
    #[error("audio track {track_index} was selected more than once")]
    DuplicateTrack { track_index: usize },
    #[error("audio track {track_index} has no selected decoder")]
    MissingDecoder { track_index: usize },
    #[error("{selected} audio tracks were selected; maximum is {limit}")]
    TooManyTracks { selected: usize, limit: usize },
    #[error("indexed Opus duration is zero")]
    ZeroIndexedDuration,
    #[error("Opus packet has {decoded} frames but the index requires {indexed}")]
    DecodedFrameShorterThanIndex { decoded: usize, indexed: usize },
    #[error("decoded Opus output has {samples} samples, not divisible by {channels} channels")]
    DecodedChannelMismatch { samples: usize, channels: u8 },
    #[error("audio packet ranges select track {track_index} more than once")]
    DuplicatePacketRange { track_index: usize },
    #[error("audio packet range {start}..{end} is invalid for track {track_index} with {samples} samples")]
    InvalidPacketRange {
        track_index: usize,
        start: usize,
        end: usize,
        samples: usize,
    },
    #[error("track {track_index} is not an Opus audio track")]
    NotOpusTrack { track_index: usize },
    #[error("track {track_index} uses timescale {timescale}; expected 48000")]
    IncompatibleTrackTimescale { track_index: usize, timescale: u32 },
    #[error("audio timeline end has a zero timescale")]
    InvalidTimelineEnd,
    #[error("track {track_index} sample {sample_index} is outside the permitted packet range")]
    PacketOutsideRange {
        track_index: usize,
        sample_index: usize,
    },
    #[error("track {track_index} sample {sample_index} begins at or beyond the timeline end")]
    PacketBeyondTimeline {
        track_index: usize,
        sample_index: usize,
    },
    #[error(transparent)]
    Index(#[from] clipline_mp4::PlaybackIndexError),
}

#[derive(Debug)]
pub struct OpusTrackDecoder {
    channels: u8,
    pre_skip: usize,
    pre_skip_remaining: usize,
    generation: WorkGeneration,
    decoder: Decoder,
    stereo_output: Vec<f32>,
    stats: OpusDecodeStats,
}

impl OpusTrackDecoder {
    pub fn new(
        channels: u16,
        sample_rate: u32,
        pre_skip: u16,
        generation: WorkGeneration,
    ) -> Result<Self, AudioError> {
        Self::new_at(
            channels,
            sample_rate,
            pre_skip,
            generation,
            AudioResetPoint::FileStart,
        )
    }

    fn new_at(
        channels: u16,
        sample_rate: u32,
        pre_skip: u16,
        generation: WorkGeneration,
        point: AudioResetPoint,
    ) -> Result<Self, AudioError> {
        validate_opus_format(channels, sample_rate)?;
        let channels = channels as u8;
        let decoder = Decoder::new(DecoderConfig::new(sample_rate, channels)).map_err(|error| {
            AudioError::DecoderState {
                message: error.to_string(),
            }
        })?;
        Ok(Self {
            channels,
            pre_skip: pre_skip as usize,
            pre_skip_remaining: match point {
                AudioResetPoint::FileStart => pre_skip as usize,
                AudioResetPoint::MidStream => 0,
            },
            generation,
            decoder,
            stereo_output: Vec::with_capacity(MAX_OPUS_FRAME_SAMPLES * 2),
            stats: OpusDecodeStats::default(),
        })
    }

    pub fn decode(
        &mut self,
        packet: &[u8],
        generation: WorkGeneration,
    ) -> Result<&[f32], AudioError> {
        self.decode_inner(packet, None, generation)
    }

    pub fn decode_indexed(
        &mut self,
        packet: &[u8],
        indexed_duration_frames: usize,
        generation: WorkGeneration,
    ) -> Result<&[f32], AudioError> {
        if indexed_duration_frames == 0 {
            self.stereo_output.clear();
            return Err(AudioError::ZeroIndexedDuration);
        }
        self.decode_inner(packet, Some(indexed_duration_frames), generation)
    }

    fn decode_inner(
        &mut self,
        packet: &[u8],
        indexed_duration_frames: Option<usize>,
        generation: WorkGeneration,
    ) -> Result<&[f32], AudioError> {
        self.stereo_output.clear();
        if generation != self.generation {
            return Err(AudioError::StaleGeneration {
                active: self.generation,
                actual: generation,
            });
        }
        if packet.len() > MAX_OPUS_PACKET_BYTES {
            return Err(AudioError::PacketTooLarge {
                size: packet.len(),
                limit: MAX_OPUS_PACKET_BYTES,
            });
        }
        let packet_frames = match packet_get_nb_samples(packet, PLAYBACK_SAMPLE_RATE) {
            Ok(packet_frames) => packet_frames,
            Err(error) => {
                return Err(self.record_corrupt(error.to_string(), indexed_duration_frames));
            }
        };
        if packet_frames > MAX_OPUS_FRAME_SAMPLES {
            self.record_corrupt_frames(indexed_duration_frames.unwrap_or(packet_frames));
            return Err(AudioError::DecodedFrameTooLarge {
                frames: packet_frames,
                limit: MAX_OPUS_FRAME_SAMPLES,
            });
        }
        if indexed_duration_frames.is_some_and(|indexed| packet_frames < indexed) {
            self.record_corrupt_frames(indexed_duration_frames.unwrap_or(packet_frames));
            return Err(AudioError::DecodedFrameShorterThanIndex {
                decoded: packet_frames,
                indexed: indexed_duration_frames.unwrap_or_default(),
            });
        }
        let decoded = match self.decoder.decode_f32(packet) {
            Ok(decoded) => decoded,
            Err(error) => {
                return Err(self.record_corrupt(
                    error.to_string(),
                    indexed_duration_frames.or(Some(packet_frames)),
                ));
            }
        };
        if !decoded.len().is_multiple_of(self.channels as usize) {
            self.record_corrupt_frames(indexed_duration_frames.unwrap_or(packet_frames));
            return Err(AudioError::DecodedChannelMismatch {
                samples: decoded.len(),
                channels: self.channels,
            });
        }
        let decoded_frames = decoded.len() / self.channels as usize;
        if decoded_frames > MAX_OPUS_FRAME_SAMPLES {
            self.record_corrupt_frames(indexed_duration_frames.unwrap_or(decoded_frames));
            return Err(AudioError::DecodedFrameTooLarge {
                frames: decoded_frames,
                limit: MAX_OPUS_FRAME_SAMPLES,
            });
        }
        let retained_frames = indexed_duration_frames.unwrap_or(decoded_frames);
        if decoded_frames < retained_frames {
            self.record_corrupt_frames(retained_frames);
            return Err(AudioError::DecodedFrameShorterThanIndex {
                decoded: decoded_frames,
                indexed: retained_frames,
            });
        }
        if let Some(sample_index) = decoded.iter().position(|sample| !sample.is_finite()) {
            self.record_corrupt_frames(retained_frames);
            return Err(AudioError::NonFiniteSample {
                track_input: 0,
                sample_index,
            });
        }
        let decoded = &decoded[..retained_frames * self.channels as usize];
        let skipped_frames = self.pre_skip_remaining.min(retained_frames);
        self.pre_skip_remaining -= skipped_frames;
        self.stats.pre_skip_frames += skipped_frames as u64;
        self.stats.decoded_packets += 1;
        let decoded = &decoded[skipped_frames * self.channels as usize..];
        match self.channels {
            1 => {
                for &sample in decoded {
                    self.stereo_output.extend_from_slice(&[sample, sample]);
                }
            }
            2 => self.stereo_output.extend_from_slice(decoded),
            _ => unreachable!(),
        }
        self.stats.decoded_frames = self
            .stats
            .decoded_frames
            .saturating_add((self.stereo_output.len() / 2) as u64);
        Ok(&self.stereo_output)
    }

    pub fn reset(
        &mut self,
        generation: WorkGeneration,
        point: AudioResetPoint,
    ) -> Result<(), AudioError> {
        self.decoder
            .reset()
            .map_err(|error| AudioError::DecoderState {
                message: error.to_string(),
            })?;
        self.generation = generation;
        self.pre_skip_remaining = match point {
            AudioResetPoint::FileStart => self.pre_skip,
            AudioResetPoint::MidStream => 0,
        };
        self.stereo_output.clear();
        self.stats.resets += 1;
        Ok(())
    }

    pub fn generation(&self) -> WorkGeneration {
        self.generation
    }

    pub fn output_len(&self) -> usize {
        self.stereo_output.len()
    }

    pub fn stats(&self) -> OpusDecodeStats {
        self.stats
    }

    fn adopt_generation(&mut self, generation: WorkGeneration) {
        self.generation = generation;
        self.stereo_output.clear();
    }

    fn record_corrupt(&mut self, message: String, indexed_frames: Option<usize>) -> AudioError {
        if let Some(frames) = indexed_frames {
            self.record_corrupt_frames(frames);
        } else {
            self.stats.corrupt_packets = self.stats.corrupt_packets.saturating_add(1);
            self.reset_after_corrupt();
            self.stereo_output.clear();
        }
        AudioError::Decode { message }
    }

    fn record_corrupt_frames(&mut self, indexed_frames: usize) {
        let skipped = self.pre_skip_remaining.min(indexed_frames);
        self.pre_skip_remaining -= skipped;
        self.stats.pre_skip_frames = self.stats.pre_skip_frames.saturating_add(skipped as u64);
        self.stats.corrupt_packets = self.stats.corrupt_packets.saturating_add(1);
        self.stats.corrupt_frames = self
            .stats
            .corrupt_frames
            .saturating_add(indexed_frames.saturating_sub(skipped) as u64);
        self.reset_after_corrupt();
        self.stereo_output.clear();
    }

    fn reset_after_corrupt(&mut self) {
        if self.decoder.reset().is_ok() {
            self.stats.resets = self.stats.resets.saturating_add(1);
        }
    }
}

fn validate_opus_format(channels: u16, sample_rate: u32) -> Result<(), AudioError> {
    if !matches!(channels, 1 | 2) {
        return Err(AudioError::InvalidChannels { channels });
    }
    if sample_rate != PLAYBACK_SAMPLE_RATE {
        return Err(AudioError::UnsupportedSampleRate { sample_rate });
    }
    Ok(())
}

#[derive(Debug)]
struct DecoderEntry {
    spec: AudioTrackSpec,
    decoder: OpusTrackDecoder,
}

#[derive(Debug, Default)]
pub struct OpusDecoderBank {
    decoders: BTreeMap<usize, DecoderEntry>,
    retired_stats: OpusDecodeStats,
    retired_by_track: BTreeMap<usize, OpusDecodeStats>,
}

impl OpusDecoderBank {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn select_tracks(
        &mut self,
        specs: &[AudioTrackSpec],
        generation: WorkGeneration,
        new_track_point: AudioResetPoint,
    ) -> Result<DecoderBankChange, AudioError> {
        if specs.len() > MAX_SELECTED_AUDIO_TRACKS {
            return Err(AudioError::TooManyTracks {
                selected: specs.len(),
                limit: MAX_SELECTED_AUDIO_TRACKS,
            });
        }
        let mut selected = BTreeSet::new();
        for spec in specs {
            validate_opus_format(spec.channels, spec.sample_rate)?;
            if !selected.insert(spec.track_index) {
                return Err(AudioError::DuplicateTrack {
                    track_index: spec.track_index,
                });
            }
        }

        let selection_changed = specs.len() != self.decoders.len()
            || specs.iter().any(|spec| {
                self.decoders
                    .get(&spec.track_index)
                    .is_none_or(|entry| entry.spec != *spec)
            });
        if !selection_changed {
            for entry in self.decoders.values_mut() {
                entry.decoder.adopt_generation(generation);
            }
            return Ok(DecoderBankChange {
                kept: specs.len(),
                added: 0,
                removed: 0,
            });
        }

        // A changed mix invalidates queued-ahead decoder state. Recreate every
        // participating decoder before touching the live bank, then retire the old bank.
        let mut next = BTreeMap::new();
        let mut kept = 0;
        let mut added = 0;
        for spec in specs {
            let unchanged = self
                .decoders
                .get(&spec.track_index)
                .is_some_and(|entry| entry.spec == *spec);
            let mut decoder = OpusTrackDecoder::new_at(
                spec.channels,
                spec.sample_rate,
                spec.pre_skip,
                generation,
                new_track_point,
            )?;
            if unchanged {
                decoder.stats.resets = 1;
                kept += 1;
            } else {
                added += 1;
            }
            next.insert(
                spec.track_index,
                DecoderEntry {
                    spec: *spec,
                    decoder,
                },
            );
        }
        let removed = self.decoders.len().saturating_sub(kept);
        self.retire_live_decoders();
        self.decoders = next;
        Ok(DecoderBankChange {
            kept,
            added,
            removed,
        })
    }

    pub fn reset_for_seek(
        &mut self,
        generation: WorkGeneration,
        point: AudioResetPoint,
    ) -> Result<(), AudioError> {
        let specs: Vec<_> = self.decoders.values().map(|entry| entry.spec).collect();
        let mut next = BTreeMap::new();
        for spec in &specs {
            let mut decoder = OpusTrackDecoder::new_at(
                spec.channels,
                spec.sample_rate,
                spec.pre_skip,
                generation,
                point,
            )?;
            decoder.stats.resets = 1;
            next.insert(
                spec.track_index,
                DecoderEntry {
                    spec: *spec,
                    decoder,
                },
            );
        }
        self.retire_live_decoders();
        self.decoders = next;
        Ok(())
    }

    pub fn decode(
        &mut self,
        track_index: usize,
        packet: &[u8],
        generation: WorkGeneration,
    ) -> Result<&[f32], AudioError> {
        self.decoders
            .get_mut(&track_index)
            .ok_or(AudioError::MissingDecoder { track_index })?
            .decoder
            .decode(packet, generation)
    }

    pub fn decode_indexed(
        &mut self,
        track_index: usize,
        packet: &[u8],
        indexed_duration_frames: usize,
        generation: WorkGeneration,
    ) -> Result<&[f32], AudioError> {
        self.decoders
            .get_mut(&track_index)
            .ok_or(AudioError::MissingDecoder { track_index })?
            .decoder
            .decode_indexed(packet, indexed_duration_frames, generation)
    }

    pub fn clear_pending_frames(&mut self) {
        for entry in self.decoders.values_mut() {
            entry.decoder.stereo_output.clear();
        }
    }

    pub fn pending_frames(&self, track_index: usize) -> Result<usize, AudioError> {
        let samples = self
            .decoders
            .get(&track_index)
            .ok_or(AudioError::MissingDecoder { track_index })?
            .decoder
            .stereo_output
            .len();
        Ok(samples / 2)
    }

    pub fn mix_pending_into(
        &self,
        track_indices: &[usize],
        frames: usize,
        output: &mut [f32],
    ) -> Result<MixOutcome, AudioError> {
        if frames > MAX_OPUS_FRAME_SAMPLES {
            return Err(AudioError::MixChunkTooLarge {
                frames,
                limit: MAX_OPUS_FRAME_SAMPLES,
            });
        }
        let expected = frames.checked_mul(2).ok_or(AudioError::TimelineOverflow)?;
        if output.len() != expected {
            return Err(AudioError::OutputLengthMismatch {
                expected,
                actual: output.len(),
            });
        }
        for (track_input, &track_index) in track_indices.iter().enumerate() {
            let entry = self
                .decoders
                .get(&track_index)
                .ok_or(AudioError::MissingDecoder { track_index })?;
            if entry.decoder.stereo_output.is_empty() {
                continue;
            }
            if entry.decoder.stereo_output.len() != expected {
                return Err(AudioError::FrameLengthMismatch {
                    expected,
                    actual: entry.decoder.stereo_output.len(),
                });
            }
            if let Some(sample_index) = entry
                .decoder
                .stereo_output
                .iter()
                .position(|sample| !sample.is_finite())
            {
                return Err(AudioError::NonFiniteSample {
                    track_input,
                    sample_index,
                });
            }
        }
        output.fill(0.0);
        let mut active_tracks = 0;
        for &track_index in track_indices {
            let entry = self
                .decoders
                .get(&track_index)
                .ok_or(AudioError::MissingDecoder { track_index })?;
            if entry.decoder.stereo_output.is_empty() {
                continue;
            }
            active_tracks += 1;
            for (mixed, sample) in output
                .iter_mut()
                .zip(entry.decoder.stereo_output.iter().copied())
            {
                *mixed += sample;
            }
        }
        if active_tracks > 1 {
            let scale = 1.0 / active_tracks as f32;
            for sample in output.iter_mut() {
                *sample *= scale;
            }
        }
        for sample in output.iter_mut() {
            *sample = sample.clamp(-1.0, 1.0);
        }
        Ok(MixOutcome {
            active_tracks,
            frames,
        })
    }

    pub fn decoder_stats(&self, track_index: usize) -> Option<OpusDecodeStats> {
        let mut stats = self.retired_by_track.get(&track_index).copied();
        if let Some(current) = self.decoders.get(&track_index) {
            let total = stats.get_or_insert_default();
            merge_decode_stats(total, current.decoder.stats());
        }
        stats
    }

    pub fn selected_track_indices(&self) -> Vec<usize> {
        self.decoders.keys().copied().collect()
    }

    pub fn stats(&self) -> OpusDecodeStats {
        let mut stats = self.retired_stats;
        for entry in self.decoders.values() {
            merge_decode_stats(&mut stats, entry.decoder.stats());
        }
        stats
    }

    fn retire_live_decoders(&mut self) {
        for (track_index, entry) in std::mem::take(&mut self.decoders) {
            let stats = entry.decoder.stats();
            merge_decode_stats(&mut self.retired_stats, stats);
            merge_decode_stats(self.retired_by_track.entry(track_index).or_default(), stats);
        }
    }
}

fn merge_decode_stats(total: &mut OpusDecodeStats, next: OpusDecodeStats) {
    total.decoded_packets = total.decoded_packets.saturating_add(next.decoded_packets);
    total.decoded_frames = total.decoded_frames.saturating_add(next.decoded_frames);
    total.pre_skip_frames = total.pre_skip_frames.saturating_add(next.pre_skip_frames);
    total.corrupt_packets = total.corrupt_packets.saturating_add(next.corrupt_packets);
    total.corrupt_frames = total.corrupt_frames.saturating_add(next.corrupt_frames);
    total.resets = total.resets.saturating_add(next.resets);
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AudioPacketTelemetry {
    pub packet_capacity: usize,
    pub packet_high_water: usize,
    pub packet_reserve_count: usize,
    pub packets_read: u64,
    pub logical_packet_limit: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub struct IndexedAudioPacket<'a> {
    pub bytes: &'a [u8],
    pub track_index: usize,
    pub sample_index: usize,
    pub pts: i64,
    pub audible_start_tick: u64,
    pub indexed_duration_frames: usize,
    pub generation: WorkGeneration,
}

pub struct IndexedAudioPacketReader<R: Read + Seek> {
    movie: IndexedMovie<R>,
    ranges: BTreeMap<usize, Range<usize>>,
    timeline_end_48k: u64,
    generation: WorkGeneration,
    packet: Vec<u8>,
    packet_high_water: usize,
    packet_reserve_count: usize,
    packets_read: u64,
}

struct PacketSelection {
    ranges: BTreeMap<usize, Range<usize>>,
    timeline_end_48k: u64,
    max_packet_size: usize,
}

impl<R: Read + Seek> std::fmt::Debug for IndexedAudioPacketReader<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IndexedAudioPacketReader")
            .field("ranges", &self.ranges)
            .field("timeline_end_48k", &self.timeline_end_48k)
            .field("generation", &self.generation)
            .field("telemetry", &self.telemetry())
            .finish_non_exhaustive()
    }
}

impl<R: Read + Seek> IndexedAudioPacketReader<R> {
    pub fn new(
        movie: IndexedMovie<R>,
        ranges: Vec<TrackSampleRange>,
        timeline_end: PlaybackTime,
        generation: WorkGeneration,
    ) -> Result<Self, AudioError> {
        let selection = validate_packet_selection(movie.index(), ranges, timeline_end)?;

        let mut packet = Vec::new();
        packet
            .try_reserve_exact(selection.max_packet_size)
            .map_err(|_| AudioError::PacketBufferAllocationFailed {
                requested: selection.max_packet_size,
            })?;
        Ok(Self {
            movie,
            ranges: selection.ranges,
            timeline_end_48k: selection.timeline_end_48k,
            generation,
            packet,
            packet_high_water: 0,
            packet_reserve_count: usize::from(selection.max_packet_size != 0),
            packets_read: 0,
        })
    }

    pub fn index(&self) -> &MovieIndex {
        self.movie.index()
    }

    pub fn read_packet(
        &mut self,
        track_index: usize,
        sample_index: usize,
        generation: WorkGeneration,
    ) -> Result<IndexedAudioPacket<'_>, AudioError> {
        if generation != self.generation {
            return Err(AudioError::StaleGeneration {
                active: self.generation,
                actual: generation,
            });
        }
        let range = self
            .ranges
            .get(&track_index)
            .ok_or(AudioError::PacketOutsideRange {
                track_index,
                sample_index,
            })?;
        if !range.contains(&sample_index) {
            return Err(AudioError::PacketOutsideRange {
                track_index,
                sample_index,
            });
        }
        let track = &self.movie.index().tracks[track_index];
        let sample = track.samples[sample_index].clone();
        let PlaybackTrackConfig::Opus { pre_skip, .. } = track.config else {
            return Err(AudioError::NotOpusTrack { track_index });
        };
        let first_pts = track.samples.first().map_or(0, |sample| sample.pts);
        let audible_start_tick = audible_start_tick(sample.pts, first_pts, pre_skip);
        if audible_start_tick >= self.timeline_end_48k {
            return Err(AudioError::PacketBeyondTimeline {
                track_index,
                sample_index,
            });
        }
        let size = sample.size as usize;
        if size > MAX_OPUS_PACKET_BYTES || size > self.packet.capacity() {
            return Err(AudioError::PacketTooLarge {
                size,
                limit: MAX_OPUS_PACKET_BYTES,
            });
        }
        self.packet.resize(size, 0);
        let read_size =
            match self
                .movie
                .read_sample_into(track_index, sample_index, &mut self.packet)
            {
                Ok(read_size) => read_size,
                Err(error) => {
                    self.packet.clear();
                    return Err(error.into());
                }
            };
        if read_size != size {
            self.packet.clear();
            return Err(clipline_mp4::PlaybackIndexError::InvalidSample(format!(
                "track {track_index} sample {sample_index} indexed {size} bytes but read {read_size}"
            ))
            .into());
        }
        self.packet_high_water = self.packet_high_water.max(read_size);
        self.packets_read = self.packets_read.saturating_add(1);
        Ok(IndexedAudioPacket {
            bytes: &self.packet[..read_size],
            track_index,
            sample_index,
            pts: sample.pts,
            audible_start_tick,
            indexed_duration_frames: sample.duration as usize,
            generation,
        })
    }

    pub fn reset_generation(&mut self, generation: WorkGeneration) {
        self.generation = generation;
        self.packet.clear();
    }

    pub fn reselect_ranges(
        &mut self,
        ranges: Vec<TrackSampleRange>,
        timeline_end: PlaybackTime,
        generation: WorkGeneration,
    ) -> Result<(), AudioError> {
        let selection = validate_packet_selection(self.movie.index(), ranges, timeline_end)?;
        if selection.max_packet_size > self.packet.capacity() {
            self.packet.clear();
            let prior_capacity = self.packet.capacity();
            self.packet
                .try_reserve_exact(selection.max_packet_size)
                .map_err(|_| AudioError::PacketBufferAllocationFailed {
                    requested: selection.max_packet_size,
                })?;
            if self.packet.capacity() != prior_capacity {
                self.packet_reserve_count = self.packet_reserve_count.saturating_add(1);
            }
        }
        self.packet.clear();
        self.ranges = selection.ranges;
        self.timeline_end_48k = selection.timeline_end_48k;
        self.generation = generation;
        Ok(())
    }

    pub fn telemetry(&self) -> AudioPacketTelemetry {
        AudioPacketTelemetry {
            packet_capacity: self.packet.capacity(),
            packet_high_water: self.packet_high_water,
            packet_reserve_count: self.packet_reserve_count,
            packets_read: self.packets_read,
            logical_packet_limit: MAX_OPUS_PACKET_BYTES,
        }
    }
}

fn validate_packet_selection(
    index: &MovieIndex,
    ranges: Vec<TrackSampleRange>,
    timeline_end: PlaybackTime,
) -> Result<PacketSelection, AudioError> {
    if timeline_end.timescale == 0 {
        return Err(AudioError::InvalidTimelineEnd);
    }
    if ranges.len() > MAX_SELECTED_AUDIO_TRACKS {
        return Err(AudioError::TooManyTracks {
            selected: ranges.len(),
            limit: MAX_SELECTED_AUDIO_TRACKS,
        });
    }
    let timeline_end_48k = rescale_time_floor(timeline_end, PLAYBACK_SAMPLE_RATE);
    let mut selected_ranges = BTreeMap::new();
    let mut max_packet_size = 0;
    for selected in ranges {
        if selected_ranges.contains_key(&selected.track_index) {
            return Err(AudioError::DuplicatePacketRange {
                track_index: selected.track_index,
            });
        }
        let track = index.tracks.get(selected.track_index).ok_or_else(|| {
            clipline_mp4::PlaybackIndexError::InvalidTrack(format!(
                "movie has no track {}",
                selected.track_index
            ))
        })?;
        let PlaybackTrackConfig::Opus {
            channels,
            sample_rate,
            pre_skip,
        } = track.config
        else {
            return Err(AudioError::NotOpusTrack {
                track_index: selected.track_index,
            });
        };
        validate_opus_format(channels, sample_rate)?;
        if track.timescale != PLAYBACK_SAMPLE_RATE {
            return Err(AudioError::IncompatibleTrackTimescale {
                track_index: selected.track_index,
                timescale: track.timescale,
            });
        }
        if selected.samples.start > selected.samples.end
            || selected.samples.end > track.samples.len()
        {
            return Err(AudioError::InvalidPacketRange {
                track_index: selected.track_index,
                start: selected.samples.start,
                end: selected.samples.end,
                samples: track.samples.len(),
            });
        }
        let first_pts = track.samples.first().map_or(0, |sample| sample.pts);
        for sample in &track.samples[selected.samples.clone()] {
            if audible_start_tick(sample.pts, first_pts, pre_skip) >= timeline_end_48k {
                continue;
            }
            let packet_size = sample.size as usize;
            if packet_size > MAX_OPUS_PACKET_BYTES {
                return Err(AudioError::PacketTooLarge {
                    size: packet_size,
                    limit: MAX_OPUS_PACKET_BYTES,
                });
            }
            max_packet_size = max_packet_size.max(packet_size);
        }
        selected_ranges.insert(selected.track_index, selected.samples);
    }
    Ok(PacketSelection {
        ranges: selected_ranges,
        timeline_end_48k,
        max_packet_size,
    })
}

fn audible_start_tick(sample_pts: i64, first_pts: i64, pre_skip: u16) -> u64 {
    sample_pts
        .saturating_sub(i64::from(pre_skip))
        .max(first_pts)
        .max(0) as u64
}

fn rescale_time_floor(time: PlaybackTime, target_timescale: u32) -> u64 {
    let scaled = u128::from(time.ticks) * u128::from(target_timescale) / u128::from(time.timescale);
    u64::try_from(scaled).unwrap_or(u64::MAX)
}

pub fn mix_stereo_frames_into(
    inputs: &[Option<&[f32]>],
    frames: usize,
    output: &mut [f32],
) -> Result<MixOutcome, AudioError> {
    if frames > MAX_OPUS_FRAME_SAMPLES {
        return Err(AudioError::MixChunkTooLarge {
            frames,
            limit: MAX_OPUS_FRAME_SAMPLES,
        });
    }
    let expected = frames.checked_mul(2).ok_or(AudioError::TimelineOverflow)?;
    if output.len() != expected {
        return Err(AudioError::OutputLengthMismatch {
            expected,
            actual: output.len(),
        });
    }
    for (track_input, input) in inputs.iter().enumerate() {
        let Some(input) = input else {
            continue;
        };
        if input.len() != expected {
            return Err(AudioError::FrameLengthMismatch {
                expected,
                actual: input.len(),
            });
        }
        if let Some(sample_index) = input.iter().position(|sample| !sample.is_finite()) {
            return Err(AudioError::NonFiniteSample {
                track_input,
                sample_index,
            });
        }
    }
    output.fill(0.0);
    let mut active_tracks = 0;
    for input in inputs.iter().flatten() {
        active_tracks += 1;
        for (mixed, sample) in output.iter_mut().zip(input.iter().copied()) {
            *mixed += sample;
        }
    }
    if active_tracks > 1 {
        let scale = 1.0 / active_tracks as f32;
        for sample in output.iter_mut() {
            *sample *= scale;
        }
    }
    for sample in output.iter_mut() {
        *sample = sample.clamp(-1.0, 1.0);
    }
    Ok(MixOutcome {
        active_tracks,
        frames,
    })
}

#[derive(Debug)]
pub struct TimelineAudioMixer {
    queue: StereoRingBuffer,
    scratch: Vec<f32>,
    cursor: u64,
    stats: TimelineAudioStats,
}

impl TimelineAudioMixer {
    pub fn new(queue_capacity_frames: usize, start_tick: u64) -> Result<Self, AudioError> {
        Ok(Self {
            queue: StereoRingBuffer::new(queue_capacity_frames)?,
            scratch: vec![0.0; MAX_OPUS_FRAME_SAMPLES * 2],
            cursor: start_tick,
            stats: TimelineAudioStats::default(),
        })
    }

    pub fn mix_at(
        &mut self,
        start_tick: u64,
        frames: usize,
        inputs: &[Option<&[f32]>],
    ) -> Result<(), AudioError> {
        if start_tick < self.cursor {
            return Err(AudioError::TimelineRegression {
                cursor: self.cursor,
                requested: start_tick,
            });
        }
        if frames > MAX_OPUS_FRAME_SAMPLES {
            return Err(AudioError::MixChunkTooLarge {
                frames,
                limit: MAX_OPUS_FRAME_SAMPLES,
            });
        }
        let gap =
            usize::try_from(start_tick - self.cursor).map_err(|_| AudioError::TimelineOverflow)?;
        let required = gap
            .checked_add(frames)
            .ok_or(AudioError::TimelineOverflow)?;
        if required > self.queue.available_frames() {
            return Err(AudioError::QueueFull {
                requested_frames: required,
                available_frames: self.queue.available_frames(),
            });
        }
        let output_len = frames.checked_mul(2).ok_or(AudioError::TimelineOverflow)?;
        let outcome = mix_stereo_frames_into(inputs, frames, &mut self.scratch[..output_len])?;
        let next_cursor = start_tick
            .checked_add(frames as u64)
            .ok_or(AudioError::TimelineOverflow)?;

        self.queue.push_silence(gap)?;
        self.queue.push_interleaved(&self.scratch[..output_len])?;
        self.cursor = next_cursor;
        self.stats.silent_frames += gap as u64;
        if outcome.active_tracks == 0 {
            self.stats.silent_frames += frames as u64;
        } else {
            self.stats.mixed_frames += frames as u64;
        }
        self.stats.timeline_frames = self.stats.timeline_frames.saturating_add(required as u64);
        self.refresh_queue_stats();
        Ok(())
    }

    pub fn finish_at(&mut self, end_tick: u64) -> Result<(), AudioError> {
        if end_tick < self.cursor {
            return Err(AudioError::TimelineRegression {
                cursor: self.cursor,
                requested: end_tick,
            });
        }
        let frames =
            usize::try_from(end_tick - self.cursor).map_err(|_| AudioError::TimelineOverflow)?;
        self.queue.push_silence(frames)?;
        self.cursor = end_tick;
        self.stats.silent_frames += frames as u64;
        self.stats.timeline_frames = self.stats.timeline_frames.saturating_add(frames as u64);
        self.refresh_queue_stats();
        Ok(())
    }

    pub fn drain_into(&mut self, output: &mut [f32]) -> Result<usize, AudioError> {
        self.queue.drain_into(output)
    }

    pub fn reset_at(&mut self, start_tick: u64) -> usize {
        let dropped = self.queue.queued_frames();
        self.queue.clear();
        self.scratch.fill(0.0);
        self.cursor = start_tick;
        self.stats.dropped_frames = self.stats.dropped_frames.saturating_add(dropped as u64);
        dropped
    }

    pub fn queued_frames(&self) -> usize {
        self.queue.queued_frames()
    }

    pub fn stats(&self) -> TimelineAudioStats {
        self.stats
    }

    fn refresh_queue_stats(&mut self) {
        self.stats.queue_high_water_frames = self.queue.telemetry().high_water_frames;
    }
}

const _: () = assert!(MAX_AUDIO_QUEUE_FRAMES == PLAYBACK_SAMPLE_RATE as usize / 2);
