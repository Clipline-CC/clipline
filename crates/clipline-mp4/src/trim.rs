//! Keyframe-aligned stream-copy trim for finalized Clipline MP4s.

use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::io::{Cursor, Read, Seek, SeekFrom, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use shiguredo_opus::{Decoder, DecoderConfig, Encoder, EncoderConfig};

use crate::walker::{children, find, walk, BoxInfo};
use crate::{
    AudioTrackConfig, FragSample, HybridMp4Writer, SourceSample, TrackConfig, VideoCodecParams,
    VideoTrackConfig,
};

/// Conservative upper bound for a finalized `moov` box that this metadata
/// reader will load into memory. Real Clipline sample tables are far smaller;
/// this rejects corrupt or hostile declarations before allocation.
const MAX_FINALIZED_MOOV_BYTES: u64 = 64 * 1024 * 1024;
/// Upper bound for per-track sample metadata. At 60 FPS this still permits
/// more than 18 hours of video while preventing tiny hostile tables from
/// expanding into multi-gigabyte allocations.
const MAX_PARSED_SAMPLES: usize = 4_000_000;
const MAX_OPUS_PACKET_BYTES: u32 = 1024 * 1024;
const MAX_OPUS_FRAME_TICKS: u32 = 5_760;
const OPUS_MIX_FRAME_TICKS: u32 = 960;
const TEMP_FILE_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);
static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq)]
pub struct TrimInfo {
    pub requested_start_s: f64,
    pub requested_end_s: f64,
    pub aligned_start_s: f64,
    pub aligned_end_s: f64,
    pub duration_s: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaTrackCounts {
    pub video: usize,
    pub audio: usize,
}

#[derive(Debug)]
pub enum TrimError {
    InvalidRange(String),
    Unsupported(String),
    Corrupt(String),
    Io(std::io::Error),
}

impl std::fmt::Display for TrimError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidRange(message) => write!(f, "invalid trim range: {message}"),
            Self::Unsupported(message) => write!(f, "unsupported mp4: {message}"),
            Self::Corrupt(message) => write!(f, "corrupt mp4: {message}"),
            Self::Io(e) => write!(f, "mp4 trim io: {e}"),
        }
    }
}

impl std::error::Error for TrimError {}

impl From<std::io::Error> for TrimError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

pub fn trim_keyframe_aligned(
    input: &[u8],
    start_s: f64,
    end_s: f64,
) -> Result<(Vec<u8>, TrimInfo), TrimError> {
    let mut out = Cursor::new(Vec::new());
    let info = trim_keyframe_aligned_to_writer(input, start_s, end_s, &mut out)?;
    Ok((out.into_inner(), info))
}

fn trim_keyframe_aligned_to_writer<W: Write + Seek>(
    input: &[u8],
    start_s: f64,
    end_s: f64,
    output: W,
) -> Result<TrimInfo, TrimError> {
    validate_range(start_s, end_s)?;
    let movie = parse_movie(input)?;
    let selection = select_trim_range(&movie, start_s, end_s)?;

    let mut selected: Vec<Vec<FragSample>> = Vec::with_capacity(movie.tracks.len());
    let mut starts: Vec<Vec<u64>> = Vec::with_capacity(movie.tracks.len());
    for (idx, track) in movie.tracks.iter().enumerate() {
        let records: Vec<&SampleRecord> = if idx == selection.video_idx {
            track.samples[selection.start_idx..selection.end_idx]
                .iter()
                .collect()
        } else {
            track
                .samples
                .iter()
                .filter(|sample| selection.contains_start(sample.start_ticks, track.timescale))
                .collect()
        };
        starts.push(
            records
                .iter()
                .map(|sample| selection.rebase_start(sample.start_ticks, track.timescale))
                .collect::<Result<_, _>>()?,
        );
        selected.push(
            records
                .iter()
                .map(|sample| sample.to_frag_sample(input))
                .collect::<Result<_, _>>()?,
        );
    }

    let tracks: Vec<TrackConfig> = movie.tracks.iter().map(|t| t.cfg.clone()).collect();
    let mut writer = HybridMp4Writer::new_multi(output, tracks)?;
    write_timed_frag_samples(&mut writer, &selected, &starts)?;
    let _ = writer.finalize()?;

    Ok(selection.info(start_s, end_s))
}

pub fn trim_keyframe_aligned_file(
    source: &Path,
    target: &Path,
    start_s: f64,
    end_s: f64,
) -> Result<TrimInfo, TrimError> {
    validate_range(start_s, end_s)?;
    reject_same_file(source, target)?;
    let mut source_file = File::open(source)?;
    let movie = parse_movie_reader(&mut source_file)?;
    let selection = select_trim_range(&movie, start_s, end_s)?;
    let mut per_track: Vec<Vec<SourceSample>> = Vec::with_capacity(movie.tracks.len());
    let mut starts: Vec<Vec<u64>> = Vec::with_capacity(movie.tracks.len());
    for (idx, track) in movie.tracks.iter().enumerate() {
        let records: Vec<&SampleRecord> = if idx == selection.video_idx {
            track.samples[selection.start_idx..selection.end_idx]
                .iter()
                .collect()
        } else {
            track
                .samples
                .iter()
                .filter(|sample| selection.contains_start(sample.start_ticks, track.timescale))
                .collect()
        };
        starts.push(
            records
                .iter()
                .map(|sample| selection.rebase_start(sample.start_ticks, track.timescale))
                .collect::<Result<_, _>>()?,
        );
        per_track.push(
            records
                .into_iter()
                .map(SampleRecord::to_source_sample)
                .collect(),
        );
    }

    let tracks: Vec<TrackConfig> = movie.tracks.iter().map(|t| t.cfg.clone()).collect();
    write_file_atomically(target, |target_file| {
        let mut writer = HybridMp4Writer::new_multi(target_file, tracks)?;
        write_timed_source_samples(&mut writer, &mut source_file, &per_track, &starts)?;
        Ok(writer.finalize()?)
    })?;
    Ok(selection.info(start_s, end_s))
}

pub fn remux_with_selected_audio_tracks_file(
    source: &Path,
    target: &Path,
    selected_audio_track_indices: &[u32],
) -> Result<(), TrimError> {
    reject_same_file(source, target)?;
    let mut source_file = File::open(source)?;
    let movie = parse_movie_reader(&mut source_file)?;
    let selected = selected_audio_index_set(&movie, selected_audio_track_indices)?;

    let mut tracks = Vec::new();
    let mut per_track = Vec::new();
    let mut starts = Vec::new();
    let mut audio_index = 0_usize;
    for track in &movie.tracks {
        let keep = match track.cfg {
            TrackConfig::Video(_) => true,
            TrackConfig::Audio(_) => {
                let keep = selected.contains(&audio_index);
                audio_index += 1;
                keep
            }
        };
        if keep {
            tracks.push(track.cfg.clone());
            starts.push(
                track
                    .samples
                    .iter()
                    .map(|sample| sample.start_ticks)
                    .collect(),
            );
            per_track.push(
                track
                    .samples
                    .iter()
                    .map(SampleRecord::to_source_sample)
                    .collect::<Vec<_>>(),
            );
        }
    }

    write_file_atomically(target, |target_file| {
        let mut writer = HybridMp4Writer::new_multi(target_file, tracks)?;
        write_timed_source_samples(&mut writer, &mut source_file, &per_track, &starts)?;
        Ok(writer.finalize()?)
    })
}

pub fn remux_with_mixed_audio_track_file(
    source: &Path,
    target: &Path,
    selected_audio_track_indices: &[u32],
) -> Result<(), TrimError> {
    reject_same_file(source, target)?;
    let mut source_file = File::open(source)?;
    let movie = parse_movie_reader(&mut source_file)?;
    let selected = selected_audio_index_set(&movie, selected_audio_track_indices)?;
    if selected.is_empty() {
        return remux_with_selected_audio_tracks_file(source, target, selected_audio_track_indices);
    }

    let selected_audio = selected_audio_tracks(&movie, &selected);
    let mut spool = OwnedTempFile::create_near(target, "mix")?;
    let mixed = mix_selected_opus_audio_tracks_to_spool(
        &mut source_file,
        &selected_audio,
        spool.file_mut(),
    )?;
    spool.file_mut().flush()?;

    let mut tracks = Vec::new();
    let mut per_track = Vec::new();
    let mut starts = Vec::new();
    for track in &movie.tracks {
        if matches!(track.cfg, TrackConfig::Video(_)) {
            tracks.push(track.cfg.clone());
            starts.push(
                track
                    .samples
                    .iter()
                    .map(|sample| sample.start_ticks)
                    .collect(),
            );
            per_track.push(
                track
                    .samples
                    .iter()
                    .map(SampleRecord::to_source_sample)
                    .collect::<Vec<_>>(),
            );
        }
    }
    if !mixed.samples.is_empty() {
        tracks.push(TrackConfig::Audio(mixed.cfg));
        per_track.push(mixed.samples);
        starts.push(mixed.start_ticks);
    }

    let video_sources = tracks
        .iter()
        .filter(|track| matches!(track, TrackConfig::Video(_)))
        .count();
    let mut sources = (0..video_sources)
        .map(|_| File::open(source))
        .collect::<Result<Vec<_>, _>>()?;
    if tracks
        .last()
        .is_some_and(|track| matches!(track, TrackConfig::Audio(_)))
    {
        sources.push(File::open(spool.path())?);
    }

    write_file_atomically(target, |target_file| {
        let mut writer = HybridMp4Writer::new_multi(target_file, tracks)?;
        let mut source_refs: Vec<&mut dyn crate::writer::ReadSeek> = sources
            .iter_mut()
            .map(|file| file as &mut dyn crate::writer::ReadSeek)
            .collect();
        write_timed_source_samples_from_sources(
            &mut writer,
            &mut source_refs,
            &per_track,
            &starts,
        )?;
        Ok(writer.finalize()?)
    })
}

pub fn remux_with_selected_audio_tracks(
    input: &[u8],
    selected_audio_track_indices: &[u32],
) -> Result<Vec<u8>, TrimError> {
    let movie = parse_movie(input)?;
    let selected = selected_audio_index_set(&movie, selected_audio_track_indices)?;

    let mut tracks = Vec::new();
    let mut selected_samples: Vec<Vec<FragSample>> = Vec::new();
    let mut starts = Vec::new();
    let mut audio_idx = 0usize;
    for track in &movie.tracks {
        let keep = match track.cfg {
            TrackConfig::Video(_) => true,
            TrackConfig::Audio(_) => {
                let keep = selected.contains(&audio_idx);
                audio_idx += 1;
                keep
            }
        };
        if !keep {
            continue;
        }
        tracks.push(track.cfg.clone());
        starts.push(
            track
                .samples
                .iter()
                .map(|sample| sample.start_ticks)
                .collect(),
        );
        selected_samples.push(
            track
                .samples
                .iter()
                .map(|sample| sample.to_frag_sample(input))
                .collect::<Result<_, _>>()?,
        );
    }

    let mut out = Cursor::new(Vec::new());
    let mut writer = HybridMp4Writer::new_multi(&mut out, tracks)?;
    write_timed_frag_samples(&mut writer, &selected_samples, &starts)?;
    let _ = writer.finalize()?;
    Ok(out.into_inner())
}

pub fn media_track_counts(input: &[u8]) -> Result<MediaTrackCounts, TrimError> {
    finalized_movie_track_counts(input)
}

pub fn media_track_counts_file(path: &Path) -> Result<MediaTrackCounts, TrimError> {
    let mut file = File::open(path)?;
    media_track_counts_reader(&mut file)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaVideoCodec {
    H264,
    Hevc,
    Av1,
}

pub fn media_video_codecs(input: &[u8]) -> Result<Vec<MediaVideoCodec>, TrimError> {
    finalized_movie_video_codecs(input)
}

pub fn media_video_codecs_file(path: &Path) -> Result<Vec<MediaVideoCodec>, TrimError> {
    let mut file = File::open(path)?;
    let moov = read_finalized_moov_bytes(&mut file)?;
    finalized_movie_video_codecs(&moov)
}

pub fn movie_duration_s_file(path: &Path) -> Result<Option<f64>, TrimError> {
    let mut file = File::open(path)?;
    let moov = read_finalized_moov_bytes(&mut file)?;
    Ok(crate::walker::movie_duration_s(&moov))
}

fn media_track_counts_reader<R: Read + Seek>(
    reader: &mut R,
) -> Result<MediaTrackCounts, TrimError> {
    let moov = read_finalized_moov_bytes(reader)?;
    finalized_movie_track_counts(&moov)
}

fn finalized_movie_track_counts(input: &[u8]) -> Result<MediaTrackCounts, TrimError> {
    let top = walk(input);
    let moov = find(&top, b"moov")
        .ok_or_else(|| TrimError::Unsupported("missing finalized moov".into()))?
        .clone();
    let moov_children = children(input, &moov);
    if find(&moov_children, b"mvex").is_some() {
        return Err(TrimError::Unsupported(
            "fragmented/unfinalized files are not trim-ready".into(),
        ));
    }

    let mut counts = MediaTrackCounts { video: 0, audio: 0 };
    for trak in moov_children.iter().filter(|b| &b.fourcc == b"trak") {
        match parse_track_cfg(input, trak)? {
            TrackConfig::Video(_) => counts.video += 1,
            TrackConfig::Audio(_) => counts.audio += 1,
        }
    }
    if counts.video == 0 && counts.audio == 0 {
        return Err(TrimError::Unsupported("no tracks found".into()));
    }
    Ok(counts)
}

fn finalized_movie_video_codecs(input: &[u8]) -> Result<Vec<MediaVideoCodec>, TrimError> {
    let top = walk(input);
    let moov = find(&top, b"moov")
        .ok_or_else(|| TrimError::Unsupported("missing finalized moov".into()))?
        .clone();
    let moov_children = children(input, &moov);
    if find(&moov_children, b"mvex").is_some() {
        return Err(TrimError::Unsupported(
            "fragmented/unfinalized files are not trim-ready".into(),
        ));
    }

    let mut codecs = Vec::new();
    for trak in moov_children.iter().filter(|b| &b.fourcc == b"trak") {
        let TrackConfig::Video(video) = parse_track_cfg(input, trak)? else {
            continue;
        };
        codecs.push(match video.codec {
            VideoCodecParams::H264 { .. } => MediaVideoCodec::H264,
            VideoCodecParams::Hevc { .. } => MediaVideoCodec::Hevc,
            VideoCodecParams::Av1 { .. } => MediaVideoCodec::Av1,
        });
    }
    Ok(codecs)
}

pub fn remux_with_mixed_audio_track(
    input: &[u8],
    selected_audio_track_indices: &[u32],
) -> Result<Vec<u8>, TrimError> {
    let movie = parse_movie(input)?;
    let selected = selected_audio_index_set(&movie, selected_audio_track_indices)?;
    if selected.is_empty() {
        return remux_with_selected_audio_tracks(input, selected_audio_track_indices);
    }

    let mut tracks = Vec::new();
    let mut selected_samples: Vec<Vec<FragSample>> = Vec::new();
    let mut starts = Vec::new();
    for track in &movie.tracks {
        if matches!(track.cfg, TrackConfig::Video(_)) {
            tracks.push(track.cfg.clone());
            starts.push(
                track
                    .samples
                    .iter()
                    .map(|sample| sample.start_ticks)
                    .collect(),
            );
            selected_samples.push(
                track
                    .samples
                    .iter()
                    .map(|sample| sample.to_frag_sample(input))
                    .collect::<Result<_, _>>()?,
            );
        }
    }

    let selected_audio_tracks = selected_audio_tracks(&movie, &selected);
    let mixed_audio = mix_selected_opus_audio_tracks(input, &selected_audio_tracks)?;
    if !mixed_audio.samples.is_empty() {
        tracks.push(TrackConfig::Audio(mixed_audio.cfg));
        selected_samples.push(mixed_audio.samples);
        starts.push(mixed_audio.start_ticks);
    }

    let mut out = Cursor::new(Vec::new());
    let mut writer = HybridMp4Writer::new_multi(&mut out, tracks)?;
    write_timed_frag_samples(&mut writer, &selected_samples, &starts)?;
    let _ = writer.finalize()?;
    Ok(out.into_inner())
}

fn selected_audio_index_set(
    movie: &ParsedMovie,
    selected_audio_track_indices: &[u32],
) -> Result<BTreeSet<usize>, TrimError> {
    let selected: BTreeSet<usize> = selected_audio_track_indices
        .iter()
        .map(|&idx| idx as usize)
        .collect();
    if selected.len() != selected_audio_track_indices.len() {
        return Err(TrimError::InvalidRange(
            "audio track selection contains duplicates".into(),
        ));
    }

    let audio_count = movie
        .tracks
        .iter()
        .filter(|track| matches!(track.cfg, TrackConfig::Audio(_)))
        .count();
    if let Some(invalid) = selected.iter().find(|&&idx| idx >= audio_count) {
        return Err(TrimError::InvalidRange(format!(
            "audio track index {invalid} is outside the clip's {audio_count} audio tracks"
        )));
    }
    Ok(selected)
}

fn selected_audio_tracks<'a>(
    movie: &'a ParsedMovie,
    selected: &BTreeSet<usize>,
) -> Vec<&'a ParsedTrack> {
    let mut audio_idx = 0usize;
    let mut tracks = Vec::new();
    for track in &movie.tracks {
        if matches!(track.cfg, TrackConfig::Audio(_)) {
            if selected.contains(&audio_idx) {
                tracks.push(track);
            }
            audio_idx += 1;
        }
    }
    tracks
}

struct MixedAudioTrack {
    cfg: AudioTrackConfig,
    samples: Vec<FragSample>,
    start_ticks: Vec<u64>,
}

struct MixedAudioSource {
    cfg: AudioTrackConfig,
    samples: Vec<SourceSample>,
    start_ticks: Vec<u64>,
}

struct DecodedAudioSample {
    start_ticks: u64,
    pcm: Vec<f32>,
}

struct AudioMixTrackState {
    next_sample: usize,
    pending: Option<DecodedAudioSample>,
    previous_source_end: Option<u64>,
    output_cursor: Option<u64>,
    remaining_pre_skip: u32,
}

fn timed_ranges(starts: &[u64], durations: &[u32]) -> Result<Vec<Range<usize>>, TrimError> {
    if starts.len() != durations.len() {
        return Err(TrimError::Corrupt(
            "timed sample start/duration count mismatch".into(),
        ));
    }
    let mut ranges = Vec::new();
    let mut range_start = 0_usize;
    for index in 0..starts.len() {
        let end = starts[index]
            .checked_add(u64::from(durations[index]))
            .ok_or_else(|| TrimError::Corrupt("sample timeline overflow".into()))?;
        if let Some(&next_start) = starts.get(index + 1) {
            if next_start < end {
                return Err(TrimError::Unsupported(
                    "overlapping or backward sample presentation times".into(),
                ));
            }
            if next_start != end {
                ranges.push(range_start..index + 1);
                range_start = index + 1;
            }
        }
    }
    if range_start < starts.len() {
        ranges.push(range_start..starts.len());
    }
    Ok(ranges)
}

fn write_timed_frag_samples<W: Write + Seek>(
    writer: &mut HybridMp4Writer<W>,
    samples: &[Vec<FragSample>],
    starts: &[Vec<u64>],
) -> Result<(), TrimError> {
    let ranges = prepare_timed_ranges(samples, starts, |sample| sample.duration)?;
    let iterations = ranges.iter().map(Vec::len).max().unwrap_or(0);
    for run_index in 0..iterations {
        for (track_index, track_ranges) in ranges.iter().enumerate() {
            if let Some(range) = track_ranges.get(run_index) {
                writer.set_track_decode_time(track_index, starts[track_index][range.start])?;
            }
        }
        let refs: Vec<&[FragSample]> = samples
            .iter()
            .zip(&ranges)
            .map(|(track, track_ranges)| {
                track_ranges
                    .get(run_index)
                    .map_or(&[][..], |range| &track[range.clone()])
            })
            .collect();
        writer.write_fragment_multi(&refs)?;
    }
    Ok(())
}

fn write_timed_source_samples<R: Read + Seek, W: Write + Seek>(
    writer: &mut HybridMp4Writer<W>,
    source: &mut R,
    samples: &[Vec<SourceSample>],
    starts: &[Vec<u64>],
) -> Result<(), TrimError> {
    let ranges = prepare_timed_ranges(samples, starts, |sample| sample.duration)?;
    let iterations = ranges.iter().map(Vec::len).max().unwrap_or(0);
    for run_index in 0..iterations {
        for (track_index, track_ranges) in ranges.iter().enumerate() {
            if let Some(range) = track_ranges.get(run_index) {
                writer.set_track_decode_time(track_index, starts[track_index][range.start])?;
            }
        }
        let refs: Vec<&[SourceSample]> = samples
            .iter()
            .zip(&ranges)
            .map(|(track, track_ranges)| {
                track_ranges
                    .get(run_index)
                    .map_or(&[][..], |range| &track[range.clone()])
            })
            .collect();
        writer.write_fragment_multi_from_source(source, &refs)?;
    }
    Ok(())
}

fn write_timed_source_samples_from_sources<W: Write + Seek>(
    writer: &mut HybridMp4Writer<W>,
    sources: &mut [&mut dyn crate::writer::ReadSeek],
    samples: &[Vec<SourceSample>],
    starts: &[Vec<u64>],
) -> Result<(), TrimError> {
    let ranges = prepare_timed_ranges(samples, starts, |sample| sample.duration)?;
    let iterations = ranges.iter().map(Vec::len).max().unwrap_or(0);
    for run_index in 0..iterations {
        for (track_index, track_ranges) in ranges.iter().enumerate() {
            if let Some(range) = track_ranges.get(run_index) {
                writer.set_track_decode_time(track_index, starts[track_index][range.start])?;
            }
        }
        let refs: Vec<&[SourceSample]> = samples
            .iter()
            .zip(&ranges)
            .map(|(track, track_ranges)| {
                track_ranges
                    .get(run_index)
                    .map_or(&[][..], |range| &track[range.clone()])
            })
            .collect();
        writer.write_fragment_multi_from_sources(sources, &refs)?;
    }
    Ok(())
}

fn prepare_timed_ranges<T>(
    samples: &[Vec<T>],
    starts: &[Vec<u64>],
    duration: impl Fn(&T) -> u32 + Copy,
) -> Result<Vec<Vec<Range<usize>>>, TrimError> {
    if samples.len() != starts.len() {
        return Err(TrimError::Corrupt(
            "timed track sample/start count mismatch".into(),
        ));
    }
    samples
        .iter()
        .zip(starts)
        .map(|(track, starts)| {
            let durations: Vec<u32> = track.iter().map(duration).collect();
            timed_ranges(starts, &durations)
        })
        .collect()
}

fn mix_selected_opus_audio_tracks_to_spool<R: Read + Seek, W: Write + Seek>(
    input: &mut R,
    selected_audio_tracks: &[&ParsedTrack],
    spool: &mut W,
) -> Result<MixedAudioSource, TrimError> {
    for track in selected_audio_tracks {
        ensure_mixable_audio_track(track)?;
    }
    let mut encoder = Encoder::new(EncoderConfig::new(48_000, 2))
        .map_err(|e| TrimError::Unsupported(format!("create Opus encoder for audio mix: {e}")))?;
    let pre_skip = encoder
        .get_lookahead()
        .map_err(|e| TrimError::Unsupported(format!("read Opus lookahead: {e}")))?;
    let mut decoders = (0..selected_audio_tracks.len())
        .map(|_| {
            Decoder::new(DecoderConfig::new(48_000, 2)).map_err(|e| {
                TrimError::Unsupported(format!("create Opus decoder for audio mix: {e}"))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut states = selected_audio_tracks
        .iter()
        .map(|track| AudioMixTrackState {
            next_sample: 0,
            pending: None,
            previous_source_end: None,
            output_cursor: None,
            remaining_pre_skip: match &track.cfg {
                TrackConfig::Audio(cfg) => u32::from(cfg.pre_skip),
                TrackConfig::Video(_) => 0,
            },
        })
        .collect::<Vec<_>>();
    let mut samples = Vec::new();
    let mut start_ticks = Vec::new();
    let mut next_frame_start = None;
    loop {
        if states.iter().all(|state| state.pending.is_none()) {
            for (track_index, track) in selected_audio_tracks.iter().enumerate() {
                load_next_audio_mix_sample(
                    input,
                    track,
                    &mut decoders[track_index],
                    &mut states[track_index],
                )?;
            }
        }
        let Some(earliest_pending) = states
            .iter()
            .filter_map(|state| state.pending.as_ref().map(|decoded| decoded.start_ticks))
            .min()
        else {
            break;
        };
        let frame_start = match next_frame_start {
            Some(cursor)
                if states.iter().any(|state| {
                    state
                        .pending
                        .as_ref()
                        .is_some_and(|decoded| decoded.start_ticks < cursor)
                }) =>
            {
                cursor
            }
            Some(cursor) => cursor.max(earliest_pending),
            None => earliest_pending,
        };
        let frame_end = frame_start
            .checked_add(u64::from(OPUS_MIX_FRAME_TICKS))
            .ok_or_else(|| TrimError::Corrupt("audio mix timeline overflow".into()))?;
        let mut mixed = vec![0.0; OPUS_MIX_FRAME_TICKS as usize * 2];
        let mut active_counts = vec![0_u32; OPUS_MIX_FRAME_TICKS as usize];

        for (track_index, track) in selected_audio_tracks.iter().enumerate() {
            loop {
                if states[track_index].pending.is_none()
                    && !load_next_audio_mix_sample(
                        input,
                        track,
                        &mut decoders[track_index],
                        &mut states[track_index],
                    )?
                {
                    break;
                }
                let decoded = states[track_index]
                    .pending
                    .as_ref()
                    .ok_or_else(|| TrimError::Corrupt("audio mix lost a decoded packet".into()))?;
                if decoded.start_ticks >= frame_end {
                    break;
                }
                let decoded_duration = u64::try_from(decoded.pcm.len() / 2)
                    .map_err(|_| TrimError::Corrupt("decoded Opus packet is too long".into()))?;
                let decoded_end = decoded
                    .start_ticks
                    .checked_add(decoded_duration)
                    .ok_or_else(|| TrimError::Corrupt("decoded audio timeline overflow".into()))?;
                if decoded_end <= frame_start {
                    return Err(TrimError::Unsupported(
                        "overlapping or backward source audio presentation times".into(),
                    ));
                }
                mix_decoded_audio_window(
                    &mut mixed,
                    &mut active_counts,
                    frame_start,
                    frame_end,
                    decoded,
                    decoded_end,
                )?;
                if decoded_end > frame_end {
                    break;
                }
                states[track_index].pending = None;
            }
        }
        normalize_mixed_pcm(&mut mixed, &active_counts)?;
        let data = encoder
            .encode_f32(&mixed)
            .map_err(|e| TrimError::Unsupported(format!("encode mixed Opus audio: {e}")))?;
        let offset = spool.stream_position()?;
        spool.write_all(&data)?;
        samples.push(SourceSample {
            offset,
            size: u32::try_from(data.len())
                .map_err(|_| TrimError::Corrupt("mixed Opus packet is too large".into()))?,
            duration: OPUS_MIX_FRAME_TICKS,
            is_sync: true,
        });
        start_ticks.push(frame_start);
        next_frame_start = Some(frame_end);
    }
    Ok(MixedAudioSource {
        cfg: AudioTrackConfig {
            channels: 2,
            sample_rate: 48_000,
            pre_skip,
        },
        samples,
        start_ticks,
    })
}

fn load_next_audio_mix_sample<R: Read + Seek>(
    input: &mut R,
    track: &ParsedTrack,
    decoder: &mut Decoder,
    state: &mut AudioMixTrackState,
) -> Result<bool, TrimError> {
    while state.pending.is_none() {
        let Some(sample) = track.samples.get(state.next_sample) else {
            return Ok(false);
        };
        let pcm = decode_opus_sample_reader(input, sample, decoder)?;
        state.next_sample += 1;
        state.pending = prepare_decoded_audio_sample(sample, pcm, state)?;
    }
    Ok(true)
}

fn prepare_decoded_audio_sample(
    sample: &SampleRecord,
    mut pcm: Vec<f32>,
    state: &mut AudioMixTrackState,
) -> Result<Option<DecodedAudioSample>, TrimError> {
    if sample.duration == 0
        || sample.duration > MAX_OPUS_FRAME_TICKS
        || !pcm.len().is_multiple_of(2)
        || pcm.len() / 2 > MAX_OPUS_FRAME_TICKS as usize
    {
        return Err(TrimError::Corrupt(
            "invalid decoded Opus sample duration".into(),
        ));
    }
    let decoded_duration = u32::try_from(pcm.len() / 2)
        .map_err(|_| TrimError::Corrupt("decoded Opus packet is too long".into()))?;
    if sample.duration.abs_diff(decoded_duration) > 1 {
        return Err(TrimError::Corrupt(
            "MP4 and decoded Opus sample durations differ".into(),
        ));
    }
    let source_end = sample
        .start_ticks
        .checked_add(u64::from(sample.duration))
        .ok_or_else(|| TrimError::Corrupt("source audio timeline overflow".into()))?;
    let gap = match state.previous_source_end {
        Some(previous_end) => sample
            .start_ticks
            .checked_sub(previous_end)
            .ok_or_else(|| {
                TrimError::Unsupported(
                    "overlapping or backward source audio presentation times".into(),
                )
            })?,
        None => 0,
    };
    let output_start = match state.output_cursor {
        Some(cursor) => cursor
            .checked_add(gap)
            .ok_or_else(|| TrimError::Corrupt("mixed audio timeline overflow".into()))?,
        None => sample.start_ticks,
    };
    state.previous_source_end = Some(source_end);

    let required_pcm = usize::try_from(sample.duration)
        .ok()
        .and_then(|duration| duration.checked_mul(2))
        .ok_or_else(|| TrimError::Corrupt("Opus sample duration is too large".into()))?;
    pcm.resize(required_pcm, 0.0);
    let skipped_ticks = state.remaining_pre_skip.min(sample.duration);
    state.remaining_pre_skip -= skipped_ticks;
    pcm.drain(..skipped_ticks as usize * 2);

    let retained_ticks = u64::try_from(pcm.len() / 2)
        .map_err(|_| TrimError::Corrupt("decoded Opus packet is too long".into()))?;
    state.output_cursor = Some(
        output_start
            .checked_add(retained_ticks)
            .ok_or_else(|| TrimError::Corrupt("mixed audio timeline overflow".into()))?,
    );
    if pcm.is_empty() {
        Ok(None)
    } else {
        Ok(Some(DecodedAudioSample {
            start_ticks: output_start,
            pcm,
        }))
    }
}

fn mix_selected_opus_audio_tracks(
    input: &[u8],
    selected_audio_tracks: &[&ParsedTrack],
) -> Result<MixedAudioTrack, TrimError> {
    let mut source = Cursor::new(input);
    let mut spool = Cursor::new(Vec::new());
    let mixed =
        mix_selected_opus_audio_tracks_to_spool(&mut source, selected_audio_tracks, &mut spool)?;
    let encoded = spool.into_inner();
    let out = mixed
        .samples
        .iter()
        .map(|sample| {
            let start = usize::try_from(sample.offset)
                .map_err(|_| TrimError::Corrupt("mixed packet offset is too large".into()))?;
            let end = start
                .checked_add(sample.size as usize)
                .ok_or_else(|| TrimError::Corrupt("mixed packet byte range overflow".into()))?;
            let data = encoded
                .get(start..end)
                .ok_or_else(|| TrimError::Corrupt("mixed packet is outside spool".into()))?
                .to_vec();
            Ok(FragSample {
                data,
                duration: sample.duration,
                is_sync: sample.is_sync,
            })
        })
        .collect::<Result<Vec<_>, TrimError>>()?;
    Ok(MixedAudioTrack {
        cfg: mixed.cfg,
        samples: out,
        start_ticks: mixed.start_ticks,
    })
}

fn ensure_mixable_audio_track(track: &ParsedTrack) -> Result<(), TrimError> {
    match &track.cfg {
        TrackConfig::Audio(AudioTrackConfig {
            channels: 2,
            sample_rate: 48_000,
            ..
        }) => Ok(()),
        TrackConfig::Audio(cfg) => Err(TrimError::Unsupported(format!(
            "audio mix requires stereo 48 kHz Opus tracks, got {} channel(s) at {} Hz",
            cfg.channels, cfg.sample_rate
        ))),
        TrackConfig::Video(_) => Err(TrimError::Unsupported(
            "audio mix received a video track".into(),
        )),
    }
}

fn decode_opus_sample_reader<R: Read + Seek>(
    input: &mut R,
    sample: &SampleRecord,
    decoder: &mut Decoder,
) -> Result<Vec<f32>, TrimError> {
    if sample.size > MAX_OPUS_PACKET_BYTES {
        return Err(TrimError::Corrupt(format!(
            "Opus packet exceeds {} byte mix limit",
            MAX_OPUS_PACKET_BYTES
        )));
    }
    let mut packet = vec![0_u8; sample.size as usize];
    input.seek(SeekFrom::Start(sample.offset as u64))?;
    input.read_exact(&mut packet)?;
    decoder
        .decode_f32(&packet)
        .map_err(|e| TrimError::Unsupported(format!("decode Opus audio for mix: {e}")))
}

fn mix_decoded_audio_window(
    mixed: &mut [f32],
    active_counts: &mut [u32],
    frame_start: u64,
    frame_end: u64,
    decoded: &DecodedAudioSample,
    decoded_end: u64,
) -> Result<(), TrimError> {
    let overlap_start = frame_start.max(decoded.start_ticks);
    let overlap_end = frame_end.min(decoded_end);
    for tick in overlap_start..overlap_end {
        let source_frame = usize::try_from(tick - decoded.start_ticks)
            .map_err(|_| TrimError::Corrupt("source audio offset is too large".into()))?;
        let output_frame = usize::try_from(tick - frame_start)
            .map_err(|_| TrimError::Corrupt("mixed audio offset is too large".into()))?;
        for channel in 0..2 {
            mixed[output_frame * 2 + channel] += decoded.pcm[source_frame * 2 + channel];
        }
        active_counts[output_frame] += 1;
    }
    Ok(())
}

fn normalize_mixed_pcm(mixed: &mut [f32], active_counts: &[u32]) -> Result<(), TrimError> {
    if mixed.len() != active_counts.len() * 2 || active_counts.iter().all(|count| *count == 0) {
        return Err(TrimError::Corrupt(
            "audio mix produced an empty frame".into(),
        ));
    }
    for (frame, &active) in mixed.as_chunks_mut::<2>().0.iter_mut().zip(active_counts) {
        if active > 1 {
            for sample in frame.iter_mut() {
                *sample /= active as f32;
            }
        }
        for sample in frame.iter_mut() {
            *sample = sample.clamp(-1.0, 1.0);
        }
    }
    Ok(())
}

fn reject_same_file(source: &Path, target: &Path) -> Result<(), TrimError> {
    let source_canonical = std::fs::canonicalize(source)?;
    let same_identity = target.exists() && files_have_same_identity(source, target)?;
    let same_path = std::fs::canonicalize(target)
        .is_ok_and(|target_canonical| source_canonical == target_canonical);
    if same_identity || same_path {
        return Err(TrimError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "MP4 source and target must be different files",
        )));
    }
    Ok(())
}

#[cfg(unix)]
fn files_have_same_identity(source: &Path, target: &Path) -> std::io::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let source = std::fs::metadata(source)?;
    let target = std::fs::metadata(target)?;
    Ok(source.dev() == target.dev() && source.ino() == target.ino())
}

#[cfg(windows)]
fn files_have_same_identity(source: &Path, target: &Path) -> std::io::Result<bool> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };

    fn identity(path: &Path) -> std::io::Result<(u32, u64)> {
        let file = File::open(path)?;
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        let result =
            unsafe { GetFileInformationByHandle(file.as_raw_handle() as HANDLE, &mut info) };
        if result == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let index = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
        Ok((info.dwVolumeSerialNumber, index))
    }

    Ok(identity(source)? == identity(target)?)
}

#[cfg(not(any(unix, windows)))]
fn files_have_same_identity(source: &Path, target: &Path) -> std::io::Result<bool> {
    Ok(std::fs::canonicalize(source)? == std::fs::canonicalize(target)?)
}

struct OwnedTempFile {
    path: PathBuf,
    file: Option<File>,
}

impl OwnedTempFile {
    fn create_near(target: &Path, purpose: &str) -> Result<Self, TrimError> {
        let file_name = target.file_name().ok_or_else(|| {
            TrimError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "MP4 target must include a file name",
            ))
        })?;
        if let Some(parent) = target.parent() {
            prune_abandoned_transform_temps(parent);
        }
        for _ in 0..128 {
            let suffix = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
            let mut temp_name = file_name.to_os_string();
            temp_name.push(format!(
                ".clipline-tmp-{purpose}-{}-{suffix}.tmp",
                std::process::id()
            ));
            let path = target.with_file_name(temp_name);
            match OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(file) => {
                    return Ok(Self {
                        path,
                        file: Some(file),
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(TrimError::Io(error)),
            }
        }
        Err(TrimError::Io(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "could not allocate a unique MP4 temporary file",
        )))
    }

    fn file_mut(&mut self) -> &mut File {
        self.file.as_mut().expect("owned temp file is open")
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn take_file(&mut self) -> File {
        self.file.take().expect("owned temp file is open")
    }

    fn disarm(mut self) {
        self.file.take();
        self.path.clear();
    }
}

fn prune_abandoned_transform_temps(directory: &Path) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        let is_transform_temp = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.contains(".clipline-tmp-") && name.ends_with(".tmp"));
        let abandoned = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age >= TEMP_FILE_MAX_AGE);
        if is_transform_temp && abandoned {
            let _ = std::fs::remove_file(path);
        }
    }
}

impl Drop for OwnedTempFile {
    fn drop(&mut self) {
        self.file.take();
        if !self.path.as_os_str().is_empty() {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn write_file_atomically(
    target: &Path,
    write: impl FnOnce(File) -> Result<File, TrimError>,
) -> Result<(), TrimError> {
    let mut temp = OwnedTempFile::create_near(target, "output")?;
    let file = temp.take_file();
    let file = write(file)?;
    file.sync_all()?;
    drop(file);
    replace_file(temp.path(), target)?;
    temp.disarm();
    Ok(())
}

#[cfg(windows)]
fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let from_w: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to_w: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    let result = unsafe {
        MoveFileExW(
            from_w.as_ptr(),
            to_w.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(windows))]
fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::rename(from, to)
}

struct ParsedMovie {
    tracks: Vec<ParsedTrack>,
}

struct TrimSelection {
    video_idx: usize,
    start_idx: usize,
    end_idx: usize,
    aligned_start_s: f64,
    aligned_end_s: f64,
    aligned_start_ticks: u64,
    aligned_end_ticks: u64,
    video_timescale: u32,
}

struct ParsedTrack {
    cfg: TrackConfig,
    timescale: u32,
    samples: Vec<SampleRecord>,
}

#[derive(Clone)]
struct SampleRecord {
    offset: usize,
    size: u32,
    duration: u32,
    is_sync: bool,
    start_ticks: u64,
}

impl ParsedTrack {
    fn track_end_s(&self) -> f64 {
        self.samples
            .last()
            .map(|s| s.end_s(self.timescale))
            .unwrap_or(0.0)
    }
}

impl SampleRecord {
    fn end_s(&self, timescale: u32) -> f64 {
        (self.start_ticks + self.duration as u64) as f64 / timescale as f64
    }

    fn to_frag_sample(&self, input: &[u8]) -> Result<FragSample, TrimError> {
        let start = self.offset;
        let end = start
            .checked_add(self.size as usize)
            .ok_or_else(|| TrimError::Corrupt("sample byte range overflow".into()))?;
        let data = input
            .get(start..end)
            .ok_or_else(|| TrimError::Corrupt("sample byte range is outside file".into()))?
            .to_vec();
        Ok(FragSample {
            data,
            duration: self.duration,
            is_sync: self.is_sync,
        })
    }

    fn to_source_sample(&self) -> SourceSample {
        SourceSample {
            offset: self.offset as u64,
            size: self.size,
            duration: self.duration,
            is_sync: self.is_sync,
        }
    }
}

impl TrimSelection {
    fn info(&self, requested_start_s: f64, requested_end_s: f64) -> TrimInfo {
        TrimInfo {
            requested_start_s,
            requested_end_s,
            aligned_start_s: self.aligned_start_s,
            aligned_end_s: self.aligned_end_s,
            duration_s: self.aligned_end_s - self.aligned_start_s,
        }
    }

    fn contains_start(&self, start_ticks: u64, timescale: u32) -> bool {
        let start = u128::from(start_ticks) * u128::from(self.video_timescale);
        let lower = u128::from(self.aligned_start_ticks) * u128::from(timescale);
        let upper = u128::from(self.aligned_end_ticks) * u128::from(timescale);
        start >= lower && start < upper
    }

    fn rebase_start(&self, start_ticks: u64, timescale: u32) -> Result<u64, TrimError> {
        let start = u128::from(start_ticks) * u128::from(self.video_timescale);
        let origin = u128::from(self.aligned_start_ticks) * u128::from(timescale);
        let delta = start.checked_sub(origin).ok_or_else(|| {
            TrimError::Corrupt("selected sample begins before trim origin".into())
        })?;
        let rounded = delta
            .checked_add(u128::from(self.video_timescale / 2))
            .ok_or_else(|| TrimError::Corrupt("trim timestamp rounding overflow".into()))?
            / u128::from(self.video_timescale);
        u64::try_from(rounded).map_err(|_| TrimError::Corrupt("trim timestamp overflow".into()))
    }
}

fn select_trim_range(
    movie: &ParsedMovie,
    start_s: f64,
    end_s: f64,
) -> Result<TrimSelection, TrimError> {
    let video_idx = movie
        .tracks
        .iter()
        .position(|t| matches!(t.cfg, TrackConfig::Video(_)))
        .ok_or_else(|| TrimError::Unsupported("missing video track".into()))?;
    let video = &movie.tracks[video_idx];
    let video_end_s = video.track_end_s();
    if start_s >= video_end_s {
        return Err(TrimError::InvalidRange("start is past the clip end".into()));
    }

    let requested_start_ticks = seconds_to_ticks_floor(start_s, video.timescale)?;
    let requested_end_ticks = seconds_to_ticks_ceil(end_s, video.timescale)?;
    let start_idx = video
        .samples
        .iter()
        .enumerate()
        .filter(|(_, s)| s.is_sync && s.start_ticks <= requested_start_ticks)
        .map(|(i, _)| i)
        .next_back()
        .or_else(|| video.samples.iter().position(|s| s.is_sync))
        .ok_or_else(|| TrimError::Unsupported("video track has no sync samples".into()))?;

    let end_idx = video
        .samples
        .iter()
        .enumerate()
        .skip(start_idx + 1)
        .find(|(_, s)| s.is_sync && s.start_ticks >= requested_end_ticks)
        .map(|(i, _)| i)
        .unwrap_or(video.samples.len());

    let aligned_start_ticks = video.samples[start_idx].start_ticks;
    let aligned_end_ticks = if end_idx < video.samples.len() {
        video.samples[end_idx].start_ticks
    } else {
        video
            .samples
            .last()
            .map_or(0, |sample| sample.start_ticks + u64::from(sample.duration))
    };
    let aligned_start_s = aligned_start_ticks as f64 / video.timescale as f64;
    let aligned_end_s = aligned_end_ticks as f64 / video.timescale as f64;
    if aligned_end_s <= aligned_start_s {
        return Err(TrimError::InvalidRange(
            "aligned range does not contain a video sample".into(),
        ));
    }
    Ok(TrimSelection {
        video_idx,
        start_idx,
        end_idx,
        aligned_start_s,
        aligned_end_s,
        aligned_start_ticks,
        aligned_end_ticks,
        video_timescale: video.timescale,
    })
}

fn seconds_to_ticks_floor(seconds: f64, timescale: u32) -> Result<u64, TrimError> {
    let ticks = seconds * f64::from(timescale);
    if !ticks.is_finite() || ticks < 0.0 || ticks > u64::MAX as f64 {
        return Err(TrimError::InvalidRange(
            "trim boundary is outside the supported timeline".into(),
        ));
    }
    Ok(ticks.floor() as u64)
}

fn seconds_to_ticks_ceil(seconds: f64, timescale: u32) -> Result<u64, TrimError> {
    let ticks = seconds * f64::from(timescale);
    if !ticks.is_finite() || ticks < 0.0 || ticks > u64::MAX as f64 {
        return Err(TrimError::InvalidRange(
            "trim boundary is outside the supported timeline".into(),
        ));
    }
    Ok(ticks.ceil() as u64)
}

fn parse_movie(input: &[u8]) -> Result<ParsedMovie, TrimError> {
    parse_movie_with_source_len(input, input.len())
}

fn parse_movie_with_source_len(input: &[u8], source_len: usize) -> Result<ParsedMovie, TrimError> {
    let top = walk(input);
    let moov = find(&top, b"moov")
        .ok_or_else(|| TrimError::Unsupported("missing finalized moov".into()))?
        .clone();
    let moov_children = children(input, &moov);
    if find(&moov_children, b"mvex").is_some() {
        return Err(TrimError::Unsupported(
            "fragmented/unfinalized files are not trim-ready".into(),
        ));
    }
    let mvhd = find(&moov_children, b"mvhd")
        .ok_or_else(|| TrimError::Unsupported("missing mvhd".into()))?;
    let movie_timescale = parse_header_timescale(input, mvhd, "mvhd")?;

    let tracks: Vec<ParsedTrack> = moov_children
        .iter()
        .filter(|b| &b.fourcc == b"trak")
        .map(|trak| parse_track(input, trak, source_len, movie_timescale))
        .collect::<Result<_, _>>()?;
    if tracks.is_empty() {
        return Err(TrimError::Unsupported("no tracks found".into()));
    }
    Ok(ParsedMovie { tracks })
}

fn parse_movie_reader<R: Read + Seek>(reader: &mut R) -> Result<ParsedMovie, TrimError> {
    let source_len = usize::try_from(reader.seek(SeekFrom::End(0))?)
        .map_err(|_| TrimError::Unsupported("source file is too large to address".into()))?;
    let moov = read_finalized_moov_bytes(reader)?;
    parse_movie_with_source_len(&moov, source_len)
}

fn parse_track(
    input: &[u8],
    trak: &BoxInfo,
    source_len: usize,
    movie_timescale: u32,
) -> Result<ParsedTrack, TrimError> {
    let cfg = parse_track_cfg(input, trak)?;
    let mdia = require_child(input, trak, b"mdia")?;
    let mdhd = require_child(input, &mdia, b"mdhd")?;
    let timescale = parse_mdhd_timescale(input, &mdhd)?;
    let minf = require_child(input, &mdia, b"minf")?;
    let stbl = require_child(input, &minf, b"stbl")?;
    let samples = parse_sample_table(input, &stbl, source_len)?;
    let samples = apply_track_edit_list(input, trak, samples, timescale, movie_timescale)?;
    if samples.is_empty() {
        return Err(TrimError::Unsupported("track has no samples".into()));
    }
    Ok(ParsedTrack {
        cfg,
        timescale,
        samples,
    })
}

#[derive(Debug, Clone, Copy)]
struct ParsedEdit {
    duration_movie_ts: u64,
    media_time: i64,
}

fn apply_track_edit_list(
    input: &[u8],
    trak: &BoxInfo,
    samples: Vec<SampleRecord>,
    track_timescale: u32,
    movie_timescale: u32,
) -> Result<Vec<SampleRecord>, TrimError> {
    let Some(edts) = child(input, trak, b"edts") else {
        return Ok(samples);
    };
    let elst = require_child(input, &edts, b"elst")?;
    let edits = parse_elst(input, &elst)?;
    if edits.is_empty() {
        return Err(TrimError::Corrupt("empty elst".into()));
    }

    let mut output = Vec::with_capacity(samples.len());
    let mut presentation_cursor_movie = 0_u64;
    let mut previous_media_end = 0_u64;
    let mut saw_media = false;
    for edit in edits {
        let presentation_start =
            rescale_ticks(presentation_cursor_movie, movie_timescale, track_timescale)?;
        presentation_cursor_movie = presentation_cursor_movie
            .checked_add(edit.duration_movie_ts)
            .ok_or_else(|| TrimError::Corrupt("edit-list duration overflow".into()))?;
        if edit.media_time == -1 {
            continue;
        }
        let media_start = u64::try_from(edit.media_time)
            .map_err(|_| TrimError::Unsupported("negative edit-list media time".into()))?;
        if saw_media && media_start < previous_media_end {
            return Err(TrimError::Unsupported(
                "overlapping or backward edit-list media ranges".into(),
            ));
        }
        let first = samples
            .iter()
            .position(|sample| sample.start_ticks == media_start)
            .ok_or_else(|| {
                TrimError::Unsupported("edit-list media time begins within a sample".into())
            })?;
        let duration_scaled = u128::from(edit.duration_movie_ts) * u128::from(track_timescale);
        let duration_scale = u128::from(movie_timescale);
        let mut copied = 0_usize;
        for sample in samples.iter().skip(first) {
            let relative_start = sample
                .start_ticks
                .checked_sub(media_start)
                .ok_or_else(|| TrimError::Unsupported("backward edit-list media range".into()))?;
            if u128::from(relative_start) * duration_scale >= duration_scaled {
                break;
            }
            let mut mapped = sample.clone();
            mapped.start_ticks = presentation_start
                .checked_add(relative_start)
                .ok_or_else(|| TrimError::Corrupt("mapped sample time overflow".into()))?;
            output.push(mapped);
            copied += 1;
        }
        if copied == 0 {
            return Err(TrimError::Unsupported(
                "edit-list media segment contains no complete sample start".into(),
            ));
        }
        previous_media_end = samples[first + copied - 1]
            .start_ticks
            .checked_add(u64::from(samples[first + copied - 1].duration))
            .ok_or_else(|| TrimError::Corrupt("sample end overflow".into()))?;
        let presented_media = previous_media_end
            .checked_sub(media_start)
            .ok_or_else(|| TrimError::Unsupported("backward edit-list media range".into()))?;
        if u128::from(presented_media) * u128::from(movie_timescale) != duration_scaled {
            return Err(TrimError::Unsupported(
                "edit-list media segment must end on a sample boundary".into(),
            ));
        }
        saw_media = true;
    }
    if output.is_empty() {
        return Err(TrimError::Unsupported(
            "edit list presents no track samples".into(),
        ));
    }
    Ok(output)
}

fn parse_elst(input: &[u8], elst: &BoxInfo) -> Result<Vec<ParsedEdit>, TrimError> {
    let p = elst.payload_offset as usize;
    let end = box_end(elst)?;
    let version = *input
        .get(p)
        .filter(|_| p < end)
        .ok_or_else(|| TrimError::Corrupt("truncated elst".into()))?;
    let entry_size = match version {
        0 => 12,
        1 => 20,
        _ => return Err(TrimError::Unsupported("unknown elst version".into())),
    };
    let count = read_u32_bounded(input, p + 4, end, "elst")? as usize;
    validate_table_entries(count, p + 8, end, entry_size, "elst")?;
    let mut edits = Vec::with_capacity(count);
    let mut pos = p + 8;
    for _ in 0..count {
        let (duration_movie_ts, media_time, rate_offset) = if version == 1 {
            (
                read_u64_bounded(input, pos, end, "elst")?,
                read_u64_bounded(input, pos + 8, end, "elst")? as i64,
                pos + 16,
            )
        } else {
            (
                u64::from(read_u32_bounded(input, pos, end, "elst")?),
                i64::from(read_u32_bounded(input, pos + 4, end, "elst")? as i32),
                pos + 8,
            )
        };
        if read_u32_bounded(input, rate_offset, end, "elst")? != 0x0001_0000 {
            return Err(TrimError::Unsupported(
                "edit-list media rates other than 1.0 are unsupported".into(),
            ));
        }
        if duration_movie_ts == 0 {
            return Err(TrimError::Corrupt("zero-duration edit-list entry".into()));
        }
        edits.push(ParsedEdit {
            duration_movie_ts,
            media_time,
        });
        pos += entry_size;
    }
    Ok(edits)
}

fn rescale_ticks(
    value: u64,
    source_timescale: u32,
    target_timescale: u32,
) -> Result<u64, TrimError> {
    let scaled = u128::from(value) * u128::from(target_timescale) / u128::from(source_timescale);
    u64::try_from(scaled).map_err(|_| TrimError::Corrupt("timestamp rescale overflow".into()))
}

fn parse_track_cfg(input: &[u8], trak: &BoxInfo) -> Result<TrackConfig, TrimError> {
    let mdia = require_child(input, trak, b"mdia")?;
    let mdhd = require_child(input, &mdia, b"mdhd")?;
    let timescale = parse_mdhd_timescale(input, &mdhd)?;
    let hdlr = require_child(input, &mdia, b"hdlr")?;
    let handler = parse_hdlr(input, &hdlr)?;
    let minf = require_child(input, &mdia, b"minf")?;
    let stbl = require_child(input, &minf, b"stbl")?;
    let stsd = require_child(input, &stbl, b"stsd")?;
    parse_stsd(input, &stsd, handler, timescale)
}

fn validate_range(start_s: f64, end_s: f64) -> Result<(), TrimError> {
    if !start_s.is_finite() || !end_s.is_finite() {
        return Err(TrimError::InvalidRange(
            "start and end must be finite".into(),
        ));
    }
    if start_s < 0.0 {
        return Err(TrimError::InvalidRange("start must be non-negative".into()));
    }
    if end_s <= start_s {
        return Err(TrimError::InvalidRange(
            "end must be greater than start".into(),
        ));
    }
    Ok(())
}

fn parse_mdhd_timescale(input: &[u8], mdhd: &BoxInfo) -> Result<u32, TrimError> {
    parse_header_timescale(input, mdhd, "mdhd")
}

fn parse_header_timescale(input: &[u8], header: &BoxInfo, label: &str) -> Result<u32, TrimError> {
    let p = header.payload_offset as usize;
    let end = box_end(header)?;
    let version = *input
        .get(p)
        .filter(|_| p < end)
        .ok_or_else(|| TrimError::Corrupt(format!("truncated {label}")))?;
    let ts_off = match version {
        0 => p + 12,
        1 => p + 20,
        _ => return Err(TrimError::Unsupported(format!("unknown {label} version"))),
    };
    let timescale = read_u32_bounded(input, ts_off, end, label)?;
    if timescale == 0 {
        return Err(TrimError::Corrupt(format!("zero {label} timescale")));
    }
    Ok(timescale)
}

fn parse_hdlr(input: &[u8], hdlr: &BoxInfo) -> Result<[u8; 4], TrimError> {
    let p = hdlr.payload_offset as usize;
    read_fourcc_bounded(input, p + 8, box_end(hdlr)?, "hdlr")
}

fn parse_stsd(
    input: &[u8],
    stsd: &BoxInfo,
    handler: [u8; 4],
    timescale: u32,
) -> Result<TrackConfig, TrimError> {
    let p = stsd.payload_offset as usize;
    let stsd_end = box_end(stsd)?;
    let entry_count = read_u32_bounded(input, p + 4, stsd_end, "stsd")?;
    if entry_count != 1 {
        return Err(TrimError::Unsupported(
            "expected exactly one sample description".into(),
        ));
    }
    let entry = read_box_at(input, p + 8, stsd_end)?;
    match &handler {
        b"vide" => parse_video_stsd(input, &entry, timescale),
        b"soun" => parse_audio_stsd(input, &entry),
        _ => Err(TrimError::Unsupported(format!(
            "unsupported handler {}",
            fourcc_str(&handler)
        ))),
    }
}

fn parse_video_stsd(
    input: &[u8],
    entry: &BoxInfo,
    timescale: u32,
) -> Result<TrackConfig, TrimError> {
    let p = entry.payload_offset as usize;
    let entry_end = box_end(entry)?;
    if p + 78 > entry_end {
        return Err(TrimError::Corrupt("truncated visual sample entry".into()));
    }
    let width = read_u16(input, p + 24)?;
    let height = read_u16(input, p + 26)?;
    // The codec configuration box follows the 78-byte VisualSampleEntry
    // shell, which is identical for avc1/hvc1/av01.
    let codec = match &entry.fourcc {
        b"avc1" => {
            let avcc = find_box_between(input, p + 78, entry_end, b"avcC")?
                .ok_or_else(|| TrimError::Unsupported("missing avcC".into()))?;
            let (sps, pps) = parse_avcc(input, &avcc)?;
            VideoCodecParams::H264 { sps, pps }
        }
        b"hvc1" | b"hev1" => {
            let hvcc = find_box_between(input, p + 78, entry_end, b"hvcC")?
                .ok_or_else(|| TrimError::Unsupported("missing hvcC".into()))?;
            let (vps, sps, pps) = parse_hvcc(input, &hvcc)?;
            VideoCodecParams::Hevc { vps, sps, pps }
        }
        b"av01" => {
            let av1c = find_box_between(input, p + 78, entry_end, b"av1C")?
                .ok_or_else(|| TrimError::Unsupported("missing av1C".into()))?;
            let sequence_header_obu = parse_av1c(input, &av1c)?;
            VideoCodecParams::Av1 {
                sequence_header_obu,
            }
        }
        other => {
            return Err(TrimError::Unsupported(format!(
                "unsupported video sample entry {}",
                fourcc_str(other)
            )))
        }
    };
    Ok(TrackConfig::Video(VideoTrackConfig {
        width,
        height,
        timescale,
        codec,
    }))
}

fn parse_audio_stsd(input: &[u8], entry: &BoxInfo) -> Result<TrackConfig, TrimError> {
    if &entry.fourcc != b"Opus" {
        return Err(TrimError::Unsupported(format!(
            "unsupported audio sample entry {}",
            fourcc_str(&entry.fourcc)
        )));
    }
    let p = entry.payload_offset as usize;
    let entry_end = box_end(entry)?;
    if p + 28 > entry_end {
        return Err(TrimError::Corrupt("truncated Opus sample entry".into()));
    }
    let channels = read_u16(input, p + 16)?;
    let dops = find_box_between(input, p + 28, entry_end, b"dOps")?
        .ok_or_else(|| TrimError::Unsupported("missing dOps".into()))?;
    let dp = dops.payload_offset as usize;
    let dops_end = box_end(&dops)?;
    let pre_skip = read_u16_bounded(input, dp + 2, dops_end, "dOps")?;
    let sample_rate = read_u32_bounded(input, dp + 4, dops_end, "dOps")?;
    Ok(TrackConfig::Audio(AudioTrackConfig {
        channels,
        sample_rate,
        pre_skip,
    }))
}

type H264ParamSets = (Vec<Vec<u8>>, Vec<Vec<u8>>);

fn parse_avcc(input: &[u8], avcc: &BoxInfo) -> Result<H264ParamSets, TrimError> {
    let p = avcc.payload_offset as usize;
    let end = box_end(avcc)?;
    if p + 7 > end {
        return Err(TrimError::Corrupt("truncated avcC".into()));
    }
    let sps_count = input[p + 5] & 0x1f;
    if sps_count == 0 {
        return Err(TrimError::Unsupported("avcC has no SPS".into()));
    }
    let mut pos = p + 6;
    let mut sps = Vec::with_capacity(sps_count as usize);
    for _ in 0..sps_count {
        let len = read_u16_bounded(input, pos, end, "avcC")? as usize;
        pos += 2;
        let data = read_slice(input, pos, len, end)?.to_vec();
        pos += len;
        sps.push(data);
    }
    let pps_count = *input
        .get(pos)
        .ok_or_else(|| TrimError::Corrupt("truncated avcC PPS count".into()))?;
    pos += 1;
    if pps_count == 0 {
        return Err(TrimError::Unsupported("avcC has no PPS".into()));
    }
    let mut pps = Vec::with_capacity(pps_count as usize);
    for _ in 0..pps_count {
        let pps_len = read_u16_bounded(input, pos, end, "avcC")? as usize;
        pos += 2;
        pps.push(read_slice(input, pos, pps_len, end)?.to_vec());
        pos += pps_len;
    }
    Ok((sps, pps))
}

/// (VPS, SPS, PPS) raw NAL units recovered from an `hvcC` record.
type HevcParamSets = (Vec<Vec<u8>>, Vec<Vec<u8>>, Vec<Vec<u8>>);

/// Recover every VPS/SPS/PPS NAL from an `hvcC` NAL-array section.
fn parse_hvcc(input: &[u8], hvcc: &BoxInfo) -> Result<HevcParamSets, TrimError> {
    let p = hvcc.payload_offset as usize;
    let end = box_end(hvcc)?;
    // The fixed configuration prefix is 22 bytes; numOfArrays is the 23rd.
    if p + 23 > end {
        return Err(TrimError::Corrupt("truncated hvcC".into()));
    }
    let num_arrays = input[p + 22];
    let mut pos = p + 23;
    let mut vps = Vec::new();
    let mut sps = Vec::new();
    let mut pps = Vec::new();
    for _ in 0..num_arrays {
        let nal_type = *input
            .get(pos)
            .ok_or_else(|| TrimError::Corrupt("truncated hvcC array header".into()))?
            & 0x3F;
        pos += 1;
        let num_nalus = read_u16_bounded(input, pos, end, "hvcC")?;
        pos += 2;
        for _ in 0..num_nalus {
            let len = read_u16_bounded(input, pos, end, "hvcC")? as usize;
            pos += 2;
            let data = read_slice(input, pos, len, end)?.to_vec();
            pos += len;
            match nal_type {
                32 => vps.push(data),
                33 => sps.push(data),
                34 => pps.push(data),
                _ => {}
            }
        }
    }
    if vps.is_empty() || sps.is_empty() || pps.is_empty() {
        Err(TrimError::Unsupported("hvcC missing VPS/SPS/PPS".into()))
    } else {
        Ok((vps, sps, pps))
    }
}

/// The `av1C` configOBUs payload is the sequence-header OBU verbatim.
fn parse_av1c(input: &[u8], av1c: &BoxInfo) -> Result<Vec<u8>, TrimError> {
    let p = av1c.payload_offset as usize;
    let end = box_end(av1c)?;
    // 4-byte fixed configuration record, then configOBUs.
    if p + 4 > end {
        return Err(TrimError::Corrupt("truncated av1C".into()));
    }
    let obu = read_slice(input, p + 4, end - (p + 4), end)?.to_vec();
    if obu.is_empty() {
        return Err(TrimError::Unsupported("av1C has no configOBUs".into()));
    }
    Ok(obu)
}

fn parse_sample_table(
    input: &[u8],
    stbl: &BoxInfo,
    source_len: usize,
) -> Result<Vec<SampleRecord>, TrimError> {
    let stsz = require_child(input, stbl, b"stsz")?;
    let sizes = parse_stsz(input, &stsz)?;
    let stts = require_child(input, stbl, b"stts")?;
    let durations = parse_stts(input, &stts, sizes.len())?;
    let sync = match child(input, stbl, b"stss") {
        Some(stss) => parse_stss(input, &stss, sizes.len())?,
        None => vec![true; sizes.len()],
    };
    let stsc = require_child(input, stbl, b"stsc")?;
    let chunk_offsets = if let Some(co64) = child(input, stbl, b"co64") {
        parse_co64(input, &co64)?
    } else {
        let stco = require_child(input, stbl, b"stco")?;
        parse_stco(input, &stco)?
    };
    let samples_per_chunk = parse_stsc(input, &stsc, chunk_offsets.len())?;
    records_from_tables(
        source_len,
        &sizes,
        &durations,
        &sync,
        &chunk_offsets,
        &samples_per_chunk,
    )
}

fn parse_stts(
    input: &[u8],
    stts: &BoxInfo,
    expected_sample_count: usize,
) -> Result<Vec<u32>, TrimError> {
    let p = stts.payload_offset as usize;
    let count = read_u32(input, p + 4)? as usize;
    let end = box_end(stts)?;
    let mut pos = p + 8;
    validate_table_entries(count, pos, end, 8, "stts")?;
    validate_sample_count(expected_sample_count, "stts")?;
    let mut out = Vec::with_capacity(expected_sample_count);
    for _ in 0..count {
        let sample_count = read_u32(input, pos)? as usize;
        let delta = read_u32(input, pos + 4)?;
        let expanded = out
            .len()
            .checked_add(sample_count)
            .ok_or_else(|| TrimError::Corrupt("stts sample count overflow".into()))?;
        if expanded > expected_sample_count || expanded > MAX_PARSED_SAMPLES {
            return Err(TrimError::Corrupt(
                "stts sample count exceeds limit or stsz count".into(),
            ));
        }
        out.extend(std::iter::repeat_n(delta, sample_count));
        pos += 8;
    }
    if out.len() != expected_sample_count {
        return Err(TrimError::Corrupt(format!(
            "stts/stsz sample count mismatch: {} vs {expected_sample_count}",
            out.len()
        )));
    }
    Ok(out)
}

fn parse_stsz(input: &[u8], stsz: &BoxInfo) -> Result<Vec<u32>, TrimError> {
    let p = stsz.payload_offset as usize;
    let sample_size = read_u32(input, p + 4)?;
    let sample_count = read_u32(input, p + 8)? as usize;
    validate_sample_count(sample_count, "stsz")?;
    if sample_size != 0 {
        return Ok(vec![sample_size; sample_count]);
    }
    let end = box_end(stsz)?;
    let mut pos = p + 12;
    validate_table_entries(sample_count, pos, end, 4, "stsz")?;
    let mut out = Vec::with_capacity(sample_count);
    for _ in 0..sample_count {
        out.push(read_u32(input, pos)?);
        pos += 4;
    }
    Ok(out)
}

fn parse_stss(input: &[u8], stss: &BoxInfo, sample_count: usize) -> Result<Vec<bool>, TrimError> {
    let p = stss.payload_offset as usize;
    let entry_count = read_u32(input, p + 4)? as usize;
    let end = box_end(stss)?;
    let mut pos = p + 8;
    validate_table_entries(entry_count, pos, end, 4, "stss")?;
    if entry_count > sample_count {
        return Err(TrimError::Corrupt(
            "stss entry count exceeds sample count".into(),
        ));
    }
    let mut sync = vec![false; sample_count];
    for _ in 0..entry_count {
        let n = read_u32(input, pos)? as usize;
        if n == 0 || n > sample_count {
            return Err(TrimError::Corrupt("stss sample number out of range".into()));
        }
        sync[n - 1] = true;
        pos += 4;
    }
    Ok(sync)
}

fn parse_co64(input: &[u8], co64: &BoxInfo) -> Result<Vec<u64>, TrimError> {
    let p = co64.payload_offset as usize;
    let count = read_u32(input, p + 4)? as usize;
    let end = box_end(co64)?;
    let mut pos = p + 8;
    validate_sample_count(count, "co64")?;
    validate_table_entries(count, pos, end, 8, "co64")?;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        out.push(read_u64(input, pos)?);
        pos += 8;
    }
    Ok(out)
}

fn parse_stco(input: &[u8], stco: &BoxInfo) -> Result<Vec<u64>, TrimError> {
    let p = stco.payload_offset as usize;
    let count = read_u32(input, p + 4)? as usize;
    let end = box_end(stco)?;
    let mut pos = p + 8;
    validate_sample_count(count, "stco")?;
    validate_table_entries(count, pos, end, 4, "stco")?;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        out.push(read_u32(input, pos)? as u64);
        pos += 4;
    }
    Ok(out)
}

fn parse_stsc(input: &[u8], stsc: &BoxInfo, chunk_count: usize) -> Result<Vec<u32>, TrimError> {
    let p = stsc.payload_offset as usize;
    let entry_count = read_u32(input, p + 4)? as usize;
    if entry_count == 0 && chunk_count > 0 {
        return Err(TrimError::Corrupt("stsc has no entries".into()));
    }
    let end = box_end(stsc)?;
    let mut pos = p + 8;
    validate_sample_count(entry_count, "stsc")?;
    validate_table_entries(entry_count, pos, end, 12, "stsc")?;
    let mut entries = Vec::with_capacity(entry_count);
    for _ in 0..entry_count {
        let first_chunk = read_u32(input, pos)?;
        let samples_per_chunk = read_u32(input, pos + 4)?;
        if first_chunk == 0 || samples_per_chunk == 0 {
            return Err(TrimError::Corrupt("invalid stsc entry".into()));
        }
        entries.push((first_chunk, samples_per_chunk));
        pos += 12;
    }
    if chunk_count == 0 {
        return Ok(Vec::new());
    }
    if entries.first().map(|e| e.0) != Some(1) {
        return Err(TrimError::Corrupt(
            "first stsc entry must start at chunk 1".into(),
        ));
    }

    let mut out = Vec::with_capacity(chunk_count);
    let mut entry_idx = 0usize;
    for chunk_number in 1..=chunk_count as u32 {
        while entry_idx + 1 < entries.len() && entries[entry_idx + 1].0 <= chunk_number {
            entry_idx += 1;
        }
        out.push(entries[entry_idx].1);
    }
    Ok(out)
}

fn records_from_tables(
    source_len: usize,
    sizes: &[u32],
    durations: &[u32],
    sync: &[bool],
    chunk_offsets: &[u64],
    samples_per_chunk: &[u32],
) -> Result<Vec<SampleRecord>, TrimError> {
    let expected: usize = samples_per_chunk.iter().map(|&n| n as usize).sum();
    if expected != sizes.len() {
        return Err(TrimError::Corrupt(format!(
            "stsc sample count {expected} does not match stsz count {}",
            sizes.len()
        )));
    }

    let mut out = Vec::with_capacity(sizes.len());
    let mut sample_index = 0usize;
    let mut start_ticks = 0u64;
    for (&chunk_offset, &chunk_samples) in chunk_offsets.iter().zip(samples_per_chunk) {
        let mut offset = usize::try_from(chunk_offset)
            .map_err(|_| TrimError::Corrupt("chunk offset too large".into()))?;
        for _ in 0..chunk_samples {
            let size = sizes[sample_index];
            let end = offset
                .checked_add(size as usize)
                .ok_or_else(|| TrimError::Corrupt("sample offset overflow".into()))?;
            if end > source_len {
                return Err(TrimError::Corrupt(
                    "sample points outside source file".into(),
                ));
            }
            out.push(SampleRecord {
                offset,
                size,
                duration: durations[sample_index],
                is_sync: sync[sample_index],
                start_ticks,
            });
            start_ticks += durations[sample_index] as u64;
            offset = end;
            sample_index += 1;
        }
    }
    Ok(out)
}

fn child(input: &[u8], parent: &BoxInfo, fourcc: &[u8; 4]) -> Option<BoxInfo> {
    children(input, parent)
        .into_iter()
        .find(|b| &b.fourcc == fourcc)
}

fn require_child(input: &[u8], parent: &BoxInfo, fourcc: &[u8; 4]) -> Result<BoxInfo, TrimError> {
    child(input, parent, fourcc)
        .ok_or_else(|| TrimError::Unsupported(format!("missing {} box", fourcc_str(fourcc))))
}

fn find_box_between(
    input: &[u8],
    mut offset: usize,
    end: usize,
    fourcc: &[u8; 4],
) -> Result<Option<BoxInfo>, TrimError> {
    while offset + 8 <= end {
        let b = read_box_at(input, offset, end)?;
        let next = box_end(&b)?;
        if &b.fourcc == fourcc {
            return Ok(Some(b));
        }
        if next <= offset {
            return Err(TrimError::Corrupt("box parser made no progress".into()));
        }
        offset = next;
    }
    Ok(None)
}

fn read_box_at(input: &[u8], offset: usize, limit: usize) -> Result<BoxInfo, TrimError> {
    if offset + 8 > limit || offset + 8 > input.len() {
        return Err(TrimError::Corrupt("truncated box header".into()));
    }
    let size32 = read_u32(input, offset)?;
    let fourcc = read_fourcc(input, offset + 4)?;
    let large_size = if crate::box_header::uses_large_size(size32) {
        if offset + 16 > limit || offset + 16 > input.len() {
            return Err(TrimError::Corrupt("truncated largesize box header".into()));
        }
        Some(read_u64(input, offset + 8)?)
    } else {
        None
    };
    let decoded =
        crate::box_header::decode_box_header(size32, large_size, offset as u64, limit as u64)
            .map_err(|error| box_header_error(error, &fourcc))?;
    if decoded.end > input.len() as u64 {
        return Err(TrimError::Corrupt(format!(
            "invalid {} box size",
            fourcc_str(&fourcc)
        )));
    }
    Ok(BoxInfo {
        fourcc,
        offset: offset as u64,
        size: decoded.size,
        payload_offset: decoded.payload_offset,
    })
}

fn read_finalized_moov_bytes<R: Read + Seek>(reader: &mut R) -> Result<Vec<u8>, TrimError> {
    let file_len = reader.seek(SeekFrom::End(0))?;
    reader.seek(SeekFrom::Start(0))?;

    let mut offset = 0_u64;
    while offset < file_len {
        let top = read_top_level_box(reader, offset, file_len)?;
        if &top.fourcc == b"moov" {
            return read_box_bytes(reader, &top);
        }
        let next = top
            .offset
            .checked_add(top.size)
            .ok_or_else(|| TrimError::Corrupt("top-level box offset overflow".into()))?;
        if next <= offset {
            return Err(TrimError::Corrupt(
                "top-level box parser made no progress".into(),
            ));
        }
        offset = next;
    }

    Err(TrimError::Unsupported("missing finalized moov".into()))
}

fn read_top_level_box<R: Read + Seek>(
    reader: &mut R,
    offset: u64,
    limit: u64,
) -> Result<BoxInfo, TrimError> {
    if offset.checked_add(8).is_none_or(|end| end > limit) {
        return Err(TrimError::Corrupt("truncated box header".into()));
    }

    reader.seek(SeekFrom::Start(offset))?;
    let mut header = [0_u8; 8];
    reader.read_exact(&mut header)?;
    let size32 = u32::from_be_bytes(header[..4].try_into().unwrap());
    let fourcc = header[4..8].try_into().unwrap();

    let large_size = if crate::box_header::uses_large_size(size32) {
        if offset.checked_add(16).is_none_or(|end| end > limit) {
            return Err(TrimError::Corrupt("truncated largesize box header".into()));
        }
        let mut large = [0_u8; 8];
        reader.read_exact(&mut large)?;
        Some(u64::from_be_bytes(large))
    } else {
        None
    };
    let decoded = crate::box_header::decode_box_header(size32, large_size, offset, limit)
        .map_err(|error| box_header_error(error, &fourcc))?;

    Ok(BoxInfo {
        fourcc,
        offset,
        size: decoded.size,
        payload_offset: decoded.payload_offset,
    })
}

fn box_header_error(error: crate::box_header::BoxHeaderError, fourcc: &[u8; 4]) -> TrimError {
    match error {
        crate::box_header::BoxHeaderError::SizeOverflow => {
            TrimError::Corrupt("box size overflow".into())
        }
        crate::box_header::BoxHeaderError::MissingLargeSize
        | crate::box_header::BoxHeaderError::InvalidExtent => {
            TrimError::Corrupt(format!("invalid {} box size", fourcc_str(fourcc)))
        }
    }
}

fn read_box_bytes<R: Read + Seek>(reader: &mut R, b: &BoxInfo) -> Result<Vec<u8>, TrimError> {
    if b.size > MAX_FINALIZED_MOOV_BYTES {
        return Err(TrimError::Unsupported(format!(
            "moov box is too large to inspect ({} bytes > {} byte limit)",
            b.size, MAX_FINALIZED_MOOV_BYTES
        )));
    }
    let size = usize::try_from(b.size)
        .map_err(|_| TrimError::Unsupported("moov box is too large to inspect".into()))?;
    reader.seek(SeekFrom::Start(b.offset))?;
    let mut bytes = vec![0_u8; size];
    reader.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn box_end(b: &BoxInfo) -> Result<usize, TrimError> {
    let end = b
        .offset
        .checked_add(b.size)
        .ok_or_else(|| TrimError::Corrupt("box end offset overflow".into()))?;
    usize::try_from(end).map_err(|_| TrimError::Corrupt("box end offset too large".into()))
}

fn validate_sample_count(count: usize, table: &str) -> Result<(), TrimError> {
    if count > MAX_PARSED_SAMPLES {
        return Err(TrimError::Corrupt(format!(
            "{table} sample count exceeds limit of {MAX_PARSED_SAMPLES}"
        )));
    }
    Ok(())
}

fn validate_table_entries(
    count: usize,
    start: usize,
    end: usize,
    entry_size: usize,
    table: &str,
) -> Result<(), TrimError> {
    let byte_len = count
        .checked_mul(entry_size)
        .ok_or_else(|| TrimError::Corrupt(format!("{table} entry byte count overflow")))?;
    let required_end = start
        .checked_add(byte_len)
        .ok_or_else(|| TrimError::Corrupt(format!("{table} entry range overflow")))?;
    if required_end > end {
        return Err(TrimError::Corrupt(format!("truncated {table}")));
    }
    Ok(())
}

fn read_slice(input: &[u8], offset: usize, len: usize, limit: usize) -> Result<&[u8], TrimError> {
    let end = offset
        .checked_add(len)
        .ok_or_else(|| TrimError::Corrupt("slice offset overflow".into()))?;
    if end > limit {
        return Err(TrimError::Corrupt(
            "slice extends past containing box".into(),
        ));
    }
    input
        .get(offset..end)
        .ok_or_else(|| TrimError::Corrupt("slice extends past file".into()))
}

fn read_u16(input: &[u8], offset: usize) -> Result<u16, TrimError> {
    Ok(u16::from_be_bytes(
        input
            .get(offset..offset + 2)
            .ok_or_else(|| TrimError::Corrupt("truncated u16".into()))?
            .try_into()
            .unwrap(),
    ))
}

fn read_u16_bounded(
    input: &[u8],
    offset: usize,
    limit: usize,
    label: &str,
) -> Result<u16, TrimError> {
    let bytes = read_slice(input, offset, 2, limit)
        .map_err(|_| TrimError::Corrupt(format!("truncated {label}")))?;
    Ok(u16::from_be_bytes(bytes.try_into().unwrap()))
}

fn read_u32(input: &[u8], offset: usize) -> Result<u32, TrimError> {
    Ok(u32::from_be_bytes(
        input
            .get(offset..offset + 4)
            .ok_or_else(|| TrimError::Corrupt("truncated u32".into()))?
            .try_into()
            .unwrap(),
    ))
}

fn read_u32_bounded(
    input: &[u8],
    offset: usize,
    limit: usize,
    label: &str,
) -> Result<u32, TrimError> {
    let bytes = read_slice(input, offset, 4, limit)
        .map_err(|_| TrimError::Corrupt(format!("truncated {label}")))?;
    Ok(u32::from_be_bytes(bytes.try_into().unwrap()))
}

fn read_u64(input: &[u8], offset: usize) -> Result<u64, TrimError> {
    Ok(u64::from_be_bytes(
        input
            .get(offset..offset + 8)
            .ok_or_else(|| TrimError::Corrupt("truncated u64".into()))?
            .try_into()
            .unwrap(),
    ))
}

fn read_u64_bounded(
    input: &[u8],
    offset: usize,
    limit: usize,
    label: &str,
) -> Result<u64, TrimError> {
    let bytes = read_slice(input, offset, 8, limit)
        .map_err(|_| TrimError::Corrupt(format!("truncated {label}")))?;
    Ok(u64::from_be_bytes(bytes.try_into().unwrap()))
}

fn read_fourcc(input: &[u8], offset: usize) -> Result<[u8; 4], TrimError> {
    let mut out = [0u8; 4];
    out.copy_from_slice(
        input
            .get(offset..offset + 4)
            .ok_or_else(|| TrimError::Corrupt("truncated fourcc".into()))?,
    );
    Ok(out)
}

fn read_fourcc_bounded(
    input: &[u8],
    offset: usize,
    limit: usize,
    label: &str,
) -> Result<[u8; 4], TrimError> {
    let mut output = [0_u8; 4];
    output.copy_from_slice(
        read_slice(input, offset, 4, limit)
            .map_err(|_| TrimError::Corrupt(format!("truncated {label}")))?,
    );
    Ok(output)
}

fn fourcc_str(fourcc: &[u8; 4]) -> String {
    String::from_utf8_lossy(fourcc).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AudioTrackConfig, VideoTrackConfig};
    use shiguredo_opus::{Decoder, DecoderConfig, Encoder, EncoderConfig};
    use std::io::{Read, Seek, SeekFrom};

    #[test]
    fn bounded_scalar_reads_do_not_borrow_bytes_from_a_sibling_box() {
        let bytes = [0_u8, 0, 0, 1, 0, 0, 0, 2];
        assert!(read_u32_bounded(&bytes, 2, 4, "test box").is_err());
        assert!(read_u16_bounded(&bytes, 3, 4, "test box").is_err());
        assert!(read_fourcc_bounded(&bytes, 2, 4, "test box").is_err());
    }

    fn video_track() -> TrackConfig {
        TrackConfig::Video(VideoTrackConfig::h264(
            128,
            72,
            90_000,
            vec![0x67, 0x64, 0x00, 0x0A, 0xAC],
            vec![0x68, 0xEE, 0x38, 0x80],
        ))
    }

    fn audio_track() -> TrackConfig {
        TrackConfig::Audio(AudioTrackConfig {
            channels: 2,
            sample_rate: 48_000,
            pre_skip: 312,
        })
    }

    fn tracks() -> Vec<TrackConfig> {
        vec![video_track(), audio_track()]
    }

    fn video_gop(start: u32) -> Vec<FragSample> {
        (0..10)
            .map(|i| FragSample {
                data: format!("V{:05}", start + i).into_bytes(),
                duration: 9_000,
                is_sync: i == 0,
            })
            .collect()
    }

    fn audio_packets(start: u32) -> Vec<FragSample> {
        audio_packets_with("A", start)
    }

    fn audio_packets_with(prefix: &str, start: u32) -> Vec<FragSample> {
        (0..50)
            .map(|i| FragSample {
                data: format!("{prefix}{:05}", start + i).into_bytes(),
                duration: 960,
                is_sync: true,
            })
            .collect()
    }

    fn opus_audio_packets(amplitude: f32) -> Vec<FragSample> {
        let mut encoder = Encoder::new(EncoderConfig::new(48_000, 2)).unwrap();
        (0..50)
            .map(|frame_idx| {
                let mut pcm = Vec::with_capacity(960 * 2);
                for sample_idx in 0..960 {
                    let t = (frame_idx * 960 + sample_idx) as f32 / 48_000.0;
                    let sample = (t * 440.0 * std::f32::consts::TAU).sin() * amplitude;
                    pcm.extend([sample, sample]);
                }
                let encoded = encoder.encode_f32(&pcm).unwrap();
                FragSample {
                    data: encoded,
                    duration: 960,
                    is_sync: true,
                }
            })
            .collect()
    }

    fn clipline_two_real_opus_audio_fixture() -> Vec<u8> {
        let mut w =
            HybridMp4Writer::new_multi(Cursor::new(Vec::new()), tracks_two_audio()).unwrap();
        let v = video_gop(0);
        let output = opus_audio_packets(0.20);
        let mic = opus_audio_packets(0.25);
        w.write_fragment_multi(&[&v, &output, &mic]).unwrap();
        w.finalize().unwrap().into_inner()
    }

    fn clipline_staggered_opus_audio_fixture() -> Vec<u8> {
        let mut w =
            HybridMp4Writer::new_multi(Cursor::new(Vec::new()), tracks_two_audio()).unwrap();
        let v = video_gop(0);
        let output = opus_audio_packets(0.20);
        let mic = opus_audio_packets(0.25);
        w.set_track_decode_time(2, 480).unwrap();
        w.write_fragment_multi(&[&v, &output, &mic]).unwrap();
        w.finalize().unwrap().into_inner()
    }

    fn clipline_gapped_opus_audio_fixture() -> Vec<u8> {
        let mut w =
            HybridMp4Writer::new_multi(Cursor::new(Vec::new()), tracks_two_audio()).unwrap();
        let v = video_gop(0);
        let output = opus_audio_packets(0.20);
        let mic = opus_audio_packets(0.25);
        w.write_fragment_multi(&[&v, &output[..1], &mic[..1]])
            .unwrap();
        w.set_track_decode_time(1, 480_000).unwrap();
        w.set_track_decode_time(2, 480_000).unwrap();
        w.write_fragment_multi(&[&[], &output[1..2], &mic[1..2]])
            .unwrap();
        w.finalize().unwrap().into_inner()
    }

    fn decoded_audible_audio_rms(input: &[u8]) -> f64 {
        let movie = parse_movie(input).unwrap();
        let audio = movie
            .tracks
            .iter()
            .find(|track| matches!(track.cfg, TrackConfig::Audio(_)))
            .expect("audio track");
        let cfg = match &audio.cfg {
            TrackConfig::Audio(cfg) => cfg,
            TrackConfig::Video(_) => unreachable!("selected audio track"),
        };
        let mut decoder = Decoder::new(DecoderConfig::new(48_000, 2)).unwrap();
        let mut pcm = Vec::new();
        for sample in &audio.samples {
            let sample = sample.to_frag_sample(input).unwrap();
            let decoded = decoder.decode_f32(sample.data.as_slice()).unwrap();
            pcm.extend(decoded);
        }
        let skip = cfg.pre_skip as usize * cfg.channels as usize;
        if skip < pcm.len() {
            pcm.drain(0..skip);
        }
        let energy = pcm
            .iter()
            .map(|sample| {
                let sample = *sample as f64;
                sample * sample
            })
            .sum::<f64>()
            / pcm.len() as f64;
        energy.sqrt()
    }

    fn first_audio_config(input: &[u8]) -> AudioTrackConfig {
        let movie = parse_movie(input).unwrap();
        movie
            .tracks
            .iter()
            .find_map(|track| match &track.cfg {
                TrackConfig::Audio(cfg) => Some(cfg.clone()),
                TrackConfig::Video(_) => None,
            })
            .expect("audio track")
    }

    fn clipline_fixture() -> Vec<u8> {
        let mut w = HybridMp4Writer::new_multi(Cursor::new(Vec::new()), tracks()).unwrap();
        for second in 0..3 {
            let v = video_gop(second * 10);
            let a = audio_packets(second * 50);
            w.write_fragment_multi(&[&v, &a]).unwrap();
        }
        w.finalize().unwrap().into_inner()
    }

    fn clipline_gap_fixture() -> Vec<u8> {
        let mut writer = HybridMp4Writer::new_multi(Cursor::new(Vec::new()), tracks()).unwrap();
        let empty: &[FragSample] = &[];
        writer
            .write_fragment_multi(&[&video_gop(0), empty])
            .unwrap();
        writer.set_track_decode_time(1, 47_520).unwrap();
        writer
            .write_fragment_multi(&[&video_gop(10), &audio_packets(0)])
            .unwrap();
        writer
            .write_fragment_multi(&[&video_gop(20), empty])
            .unwrap();
        writer.set_track_decode_time(1, 144_000).unwrap();
        writer
            .write_fragment_multi(&[&video_gop(30), &audio_packets(50)])
            .unwrap();
        writer.finalize().unwrap().into_inner()
    }

    fn tracks_two_audio() -> Vec<TrackConfig> {
        vec![video_track(), audio_track(), audio_track()]
    }

    fn audio_only_tracks() -> Vec<TrackConfig> {
        vec![audio_track()]
    }

    fn clipline_two_audio_fixture() -> Vec<u8> {
        let mut w =
            HybridMp4Writer::new_multi(Cursor::new(Vec::new()), tracks_two_audio()).unwrap();
        for second in 0..2 {
            let v = video_gop(second * 10);
            let output = audio_packets_with("A", second * 50);
            let mic = audio_packets_with("B", second * 50);
            w.write_fragment_multi(&[&v, &output, &mic]).unwrap();
        }
        w.finalize().unwrap().into_inner()
    }

    fn clipline_audio_only_fixture() -> Vec<u8> {
        let mut w =
            HybridMp4Writer::new_multi(Cursor::new(Vec::new()), audio_only_tracks()).unwrap();
        for second in 0..2 {
            let audio = audio_packets(second * 50);
            w.write_fragment_multi(&[&audio]).unwrap();
        }
        w.finalize().unwrap().into_inner()
    }

    #[test]
    fn parse_clipline_mp4_recovers_tracks_and_samples() {
        let movie = parse_movie(&clipline_fixture()).unwrap();

        assert_eq!(movie.tracks.len(), 2);
        assert_eq!(movie.tracks[0].samples.len(), 30);
        assert_eq!(movie.tracks[1].samples.len(), 150);
        assert!(movie.tracks[0].samples[0].is_sync);
        assert!(movie.tracks[0].samples[10].is_sync);
        assert!(!movie.tracks[0].samples[11].is_sync);
    }

    #[test]
    fn remux_preserves_leading_and_internal_track_gaps() {
        let fixture = clipline_gap_fixture();
        let parsed = parse_movie(&fixture).unwrap();
        assert_eq!(parsed.tracks[1].samples[0].start_ticks, 47_520);
        assert_eq!(parsed.tracks[1].samples[50].start_ticks, 144_000);

        let output = remux_with_selected_audio_tracks(&fixture, &[0]).unwrap();
        let remuxed = parse_movie(&output).unwrap();
        assert_eq!(remuxed.tracks[1].samples[0].start_ticks, 47_520);
        assert_eq!(remuxed.tracks[1].samples[50].start_ticks, 144_000);
    }

    #[test]
    fn malformed_edit_lists_are_rejected_instead_of_retimed() {
        let fixture = clipline_gap_fixture();
        let fourcc = fixture
            .windows(4)
            .position(|window| window == b"elst")
            .unwrap();
        let payload = fourcc + 4;
        let entries = payload + 8;

        let mut mid_sample = fixture.clone();
        mid_sample[entries + 12 + 4..entries + 12 + 8].copy_from_slice(&1_i32.to_be_bytes());
        assert!(parse_movie(&mid_sample).is_err());

        let mut overlapping = fixture.clone();
        overlapping[entries + 36 + 4..entries + 36 + 8].copy_from_slice(&0_i32.to_be_bytes());
        assert!(parse_movie(&overlapping).is_err());

        let mut adjusted_rate = fixture;
        adjusted_rate[entries + 8..entries + 12].copy_from_slice(&0x0002_0000_u32.to_be_bytes());
        assert!(parse_movie(&adjusted_rate).is_err());
    }

    #[test]
    fn trim_uses_integer_boundaries_without_shifting_audio_early() {
        let fixture = clipline_gap_fixture();
        let (output, info) = trim_keyframe_aligned(&fixture, 1.2, 3.2).unwrap();
        assert_eq!(info.aligned_start_s, 1.0);
        assert_eq!(info.aligned_end_s, 4.0);

        let trimmed = parse_movie(&output).unwrap();
        let audio = &trimmed.tracks[1];
        assert_eq!(
            audio.samples[0].start_ticks, 480,
            "first packet remains 10 ms late"
        );
        assert_eq!(
            audio.samples[49].start_ticks, 96_000,
            "later audio run keeps its two-second offset"
        );
    }

    #[test]
    fn rejects_unfinalized_or_missing_sample_tables() {
        let mut w = HybridMp4Writer::new_multi(Cursor::new(Vec::new()), tracks()).unwrap();
        let v = video_gop(0);
        let a = audio_packets(0);
        w.write_fragment_multi(&[&v, &a]).unwrap();
        let fragmented = w.into_inner().into_inner();

        assert!(parse_movie(&fragmented).is_err());
    }

    #[test]
    fn trims_to_previous_and_next_keyframes() {
        let (out, info) = trim_keyframe_aligned(&clipline_fixture(), 0.4, 1.2).unwrap();
        let movie = parse_movie(&out).unwrap();

        assert_eq!(info.aligned_start_s, 0.0);
        assert_eq!(info.aligned_end_s, 2.0);
        assert_eq!(movie.tracks[0].samples.len(), 20);
        assert_eq!(movie.tracks[1].samples.len(), 100);
        assert!(out.windows(6).any(|w| w == b"V00000"));
        assert!(out.windows(6).any(|w| w == b"V00019"));
        assert!(!out.windows(6).any(|w| w == b"V00020"));
    }

    // Real x265 / SVT-AV1 parameter sets (128x72) so the round-trip parses
    // genuine hvcC / av1C records, not just placeholder bytes.
    const HEVC_VPS: &[u8] = &[0x40, 0x01, 0x0C, 0x01, 0xFF, 0xFF, 0x01];
    const HEVC_SPS: &[u8] = &[
        0x42, 0x01, 0x01, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0x90, 0x00, 0x00, 0x03, 0x00, 0x00,
        0x03, 0x00, 0x1E, 0xA0, 0x10, 0x20, 0x49, 0x65, 0x95, 0x9A, 0x49, 0x32, 0xBC, 0x05, 0xA0,
        0x20, 0x00, 0x00, 0x03, 0x00, 0x20, 0x00, 0x00, 0x03, 0x03, 0xC1,
    ];
    const HEVC_PPS: &[u8] = &[0x44, 0x01, 0xC1, 0x72, 0xB4, 0x22, 0x40];
    const AV1_SEQ_OBU: &[u8] = &[
        0x0A, 0x0A, 0x00, 0x00, 0x00, 0x03, 0x37, 0xF8, 0xE3, 0x57, 0xCC, 0x02,
    ];

    fn single_video_fixture(codec: VideoCodecParams) -> Vec<u8> {
        let cfg = vec![TrackConfig::Video(VideoTrackConfig {
            width: 128,
            height: 72,
            timescale: 90_000,
            codec,
        })];
        let mut w = HybridMp4Writer::new_multi(Cursor::new(Vec::new()), cfg).unwrap();
        for second in 0..3 {
            w.write_fragment_multi(&[&video_gop(second * 10)]).unwrap();
        }
        w.finalize().unwrap().into_inner()
    }

    #[test]
    fn trims_hevc_clip_recovering_parameter_sets() {
        let second_vps = [HEVC_VPS, &[0x55]].concat();
        let second_sps = [HEVC_SPS, &[0x66]].concat();
        let second_pps = [HEVC_PPS, &[0x77]].concat();
        let fixture = single_video_fixture(VideoCodecParams::Hevc {
            vps: vec![HEVC_VPS.to_vec(), second_vps.clone()],
            sps: vec![HEVC_SPS.to_vec(), second_sps.clone()],
            pps: vec![HEVC_PPS.to_vec(), second_pps.clone()],
        });
        let (out, info) = trim_keyframe_aligned(&fixture, 0.4, 1.2).unwrap();
        let movie = parse_movie(&out).unwrap();
        assert_eq!(info.aligned_start_s, 0.0);
        assert_eq!(info.aligned_end_s, 2.0);
        assert_eq!(movie.tracks[0].samples.len(), 20);
        match &movie.tracks[0].cfg {
            TrackConfig::Video(VideoTrackConfig {
                codec: VideoCodecParams::Hevc { vps, sps, pps },
                ..
            }) => {
                assert_eq!(vps.as_slice(), &[HEVC_VPS.to_vec(), second_vps]);
                assert_eq!(sps.as_slice(), &[HEVC_SPS.to_vec(), second_sps]);
                assert_eq!(pps.as_slice(), &[HEVC_PPS.to_vec(), second_pps]);
            }
            other => panic!("expected HEVC track, got {other:?}"),
        }
        assert!(out.windows(4).any(|w| w == b"hvc1"));
    }

    #[test]
    fn remux_preserves_all_h264_parameter_sets() {
        let sps = vec![
            vec![0x67, 0x64, 0x00, 0x0A, 0xAC],
            vec![0x67, 0x64, 0x00, 0x0A, 0xAD],
        ];
        let pps = vec![vec![0x68, 0xEE, 0x38, 0x80], vec![0x68, 0xEE, 0x38, 0x81]];
        let fixture = single_video_fixture(VideoCodecParams::H264 {
            sps: sps.clone(),
            pps: pps.clone(),
        });

        let output = remux_with_selected_audio_tracks(&fixture, &[]).unwrap();
        let movie = parse_movie(&output).unwrap();
        match &movie.tracks[0].cfg {
            TrackConfig::Video(VideoTrackConfig {
                codec:
                    VideoCodecParams::H264 {
                        sps: output_sps,
                        pps: output_pps,
                    },
                ..
            }) => {
                assert_eq!(output_sps, &sps);
                assert_eq!(output_pps, &pps);
            }
            other => panic!("expected H.264 track, got {other:?}"),
        }
    }

    #[test]
    fn trims_av1_clip_recovering_sequence_header() {
        let fixture = single_video_fixture(VideoCodecParams::Av1 {
            sequence_header_obu: AV1_SEQ_OBU.to_vec(),
        });
        let (out, info) = trim_keyframe_aligned(&fixture, 0.4, 1.2).unwrap();
        let movie = parse_movie(&out).unwrap();
        assert_eq!(info.aligned_end_s, 2.0);
        assert_eq!(movie.tracks[0].samples.len(), 20);
        match &movie.tracks[0].cfg {
            TrackConfig::Video(VideoTrackConfig {
                codec:
                    VideoCodecParams::Av1 {
                        sequence_header_obu,
                    },
                ..
            }) => assert_eq!(sequence_header_obu.as_slice(), AV1_SEQ_OBU),
            other => panic!("expected AV1 track, got {other:?}"),
        }
        assert!(out.windows(4).any(|w| w == b"av01"));
    }

    #[test]
    fn file_trim_matches_in_memory_trim_output() {
        let input = clipline_fixture();
        let (expected, expected_info) = trim_keyframe_aligned(&input, 0.4, 1.2).unwrap();
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "clipline-trim-file-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("source.mp4");
        let target = dir.join("target.mp4");
        std::fs::write(&source, &input).unwrap();

        let info = trim_keyframe_aligned_file(&source, &target, 0.4, 1.2).unwrap();
        let actual = std::fs::read(&target).unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(info, expected_info);
        assert_eq!(actual, expected);
    }

    #[test]
    fn file_trim_rejects_same_source_and_target_without_truncating() {
        let input = clipline_fixture();
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "clipline-trim-same-file-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("source.mp4");
        std::fs::write(&source, &input).unwrap();

        let err = trim_keyframe_aligned_file(&source, &source, 0.4, 1.2).unwrap_err();
        let after = std::fs::read(&source).unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        assert!(matches!(
            err,
            TrimError::Io(ref e) if e.kind() == std::io::ErrorKind::InvalidInput
        ));
        assert_eq!(after, input);
    }

    #[test]
    fn file_trim_rejects_hard_link_target_without_truncating_source() {
        let input = clipline_fixture();
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "clipline-trim-hard-link-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("source.mp4");
        let target = dir.join("target.mp4");
        std::fs::write(&source, &input).unwrap();
        std::fs::hard_link(&source, &target).unwrap();

        let err = trim_keyframe_aligned_file(&source, &target, 0.4, 1.2).unwrap_err();
        let source_after = std::fs::read(&source).unwrap();
        let target_after = std::fs::read(&target).unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        assert!(matches!(
            err,
            TrimError::Io(ref e) if e.kind() == std::io::ErrorKind::InvalidInput
        ));
        assert_eq!(source_after, input);
        assert_eq!(target_after, input);
    }

    #[test]
    fn file_remux_matches_in_memory_output_and_preserves_existing_target_on_error() {
        let input = clipline_two_audio_fixture();
        let expected = remux_with_selected_audio_tracks(&input, &[1]).unwrap();
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "clipline-remux-file-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("source.mp4");
        let target = dir.join("target.mp4");
        std::fs::write(&source, &input).unwrap();

        remux_with_selected_audio_tracks_file(&source, &target, &[1]).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), expected);

        let sentinel = b"existing target must survive";
        std::fs::write(&target, sentinel).unwrap();
        let err = remux_with_selected_audio_tracks_file(&source, &target, &[2]).unwrap_err();
        assert!(err
            .to_string()
            .contains("outside the clip's 2 audio tracks"));
        assert_eq!(std::fs::read(&target).unwrap(), sentinel);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn atomic_file_transform_preserves_target_and_cleans_partial_on_late_failure() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "clipline-atomic-transform-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("target.mp4");
        std::fs::write(&target, b"previous complete clip").unwrap();

        let error = write_file_atomically(&target, |mut temporary| {
            temporary.write_all(b"partial replacement")?;
            Err(TrimError::Io(std::io::Error::other(
                "injected finalize failure",
            )))
        })
        .unwrap_err();
        let leftovers = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert!(error.to_string().contains("injected finalize failure"));
        assert_eq!(std::fs::read(&target).unwrap(), b"previous complete clip");
        assert_eq!(leftovers, vec!["target.mp4"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn abandoned_transform_temp_prune_is_scoped_and_age_gated() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "clipline-transform-prune-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let abandoned = dir.join("clip.mp4.clipline-tmp-output-1-1.tmp");
        let active = dir.join("clip.mp4.clipline-tmp-output-1-2.tmp");
        let unrelated = dir.join("editor.tmp");
        for path in [&abandoned, &active, &unrelated] {
            std::fs::write(path, b"temp").unwrap();
        }
        File::options()
            .write(true)
            .open(&abandoned)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1))
            .unwrap();

        prune_abandoned_transform_temps(&dir);

        assert!(!abandoned.exists());
        assert!(active.exists());
        assert!(unrelated.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remux_with_selected_audio_tracks_keeps_only_requested_audio() {
        let input = clipline_two_audio_fixture();

        let out = remux_with_selected_audio_tracks(&input, &[1]).unwrap();
        let movie = parse_movie(&out).unwrap();

        assert_eq!(movie.tracks.len(), 2, "video plus selected microphone");
        assert!(matches!(movie.tracks[0].cfg, TrackConfig::Video(_)));
        assert!(matches!(movie.tracks[1].cfg, TrackConfig::Audio(_)));
        assert!(out.windows(6).any(|w| w == b"V00000"));
        assert!(!out.windows(6).any(|w| w == b"A00000"));
        assert!(out.windows(6).any(|w| w == b"B00000"));
    }

    #[test]
    fn remux_with_selected_audio_tracks_can_emit_video_only() {
        let input = clipline_two_audio_fixture();

        let out = remux_with_selected_audio_tracks(&input, &[]).unwrap();
        let movie = parse_movie(&out).unwrap();

        assert_eq!(movie.tracks.len(), 1);
        assert!(matches!(movie.tracks[0].cfg, TrackConfig::Video(_)));
        assert!(out.windows(6).any(|w| w == b"V00000"));
        assert!(!out.windows(6).any(|w| w == b"A00000"));
        assert!(!out.windows(6).any(|w| w == b"B00000"));
    }

    #[test]
    fn remux_with_selected_audio_tracks_rejects_invalid_selection() {
        let input = clipline_two_audio_fixture();

        let err = remux_with_selected_audio_tracks(&input, &[2]).unwrap_err();

        assert!(
            err.to_string()
                .contains("outside the clip's 2 audio tracks"),
            "{err}"
        );
    }

    #[test]
    fn stts_rejects_an_excessive_expanded_sample_count() {
        let mut payload = vec![0_u8; 4];
        payload.extend(1_u32.to_be_bytes());
        payload.extend(4_000_001_u32.to_be_bytes());
        payload.extend(1_500_u32.to_be_bytes());
        let input = crate::boxes::mp4_box(*b"stts", payload);
        let info = walk(&input).remove(0);

        let err = parse_stts(&input, &info, 4_000_000).unwrap_err();
        assert!(err.to_string().contains("sample count exceeds limit"));
    }

    #[test]
    fn fixed_stsz_rejects_an_excessive_sample_count() {
        let mut payload = vec![0_u8; 4];
        payload.extend(1_u32.to_be_bytes());
        payload.extend(4_000_001_u32.to_be_bytes());
        let input = crate::boxes::mp4_box(*b"stsz", payload);
        let info = walk(&input).remove(0);

        let err = parse_stsz(&input, &info).unwrap_err();
        assert!(err.to_string().contains("sample count exceeds limit"));
    }

    #[test]
    fn offset_and_chunk_tables_reject_excessive_entry_counts() {
        fn table(fourcc: [u8; 4]) -> (Vec<u8>, BoxInfo) {
            let mut payload = vec![0_u8; 4];
            payload.extend(4_000_001_u32.to_be_bytes());
            let input = crate::boxes::mp4_box(fourcc, payload);
            let info = walk(&input).remove(0);
            (input, info)
        }

        let (co64, co64_info) = table(*b"co64");
        assert!(parse_co64(&co64, &co64_info)
            .unwrap_err()
            .to_string()
            .contains("sample count exceeds limit"));

        let (stco, stco_info) = table(*b"stco");
        assert!(parse_stco(&stco, &stco_info)
            .unwrap_err()
            .to_string()
            .contains("sample count exceeds limit"));

        let (stsc, stsc_info) = table(*b"stsc");
        assert!(parse_stsc(&stsc, &stsc_info, 1)
            .unwrap_err()
            .to_string()
            .contains("sample count exceeds limit"));
    }

    #[test]
    fn media_track_counts_reports_video_and_audio_tracks() {
        assert_eq!(
            media_track_counts(&clipline_two_audio_fixture()).unwrap(),
            MediaTrackCounts { video: 1, audio: 2 }
        );
        assert_eq!(
            media_track_counts(&clipline_audio_only_fixture()).unwrap(),
            MediaTrackCounts { video: 0, audio: 1 }
        );
    }

    #[test]
    fn media_video_codecs_reports_each_supported_video_sample_entry() {
        let h264 = single_video_fixture(VideoCodecParams::H264 {
            sps: vec![vec![0x67, 0x42, 0x00, 0x1f]],
            pps: vec![vec![0x68, 0xce, 0x06, 0xe2]],
        });
        let hevc = single_video_fixture(VideoCodecParams::Hevc {
            vps: vec![vec![0x40, 0x01, 0x0c]],
            sps: vec![vec![0x42, 0x01, 0x01]],
            pps: vec![vec![0x44, 0x01, 0xc0]],
        });

        assert_eq!(
            media_video_codecs(&h264).unwrap(),
            vec![MediaVideoCodec::H264]
        );
        assert_eq!(
            media_video_codecs(&hevc).unwrap(),
            vec![MediaVideoCodec::Hevc]
        );
    }

    struct TrackingCursor {
        inner: Cursor<Vec<u8>>,
        mdat_range: std::ops::Range<u64>,
        bytes_read: usize,
        seeks: Vec<u64>,
    }

    impl TrackingCursor {
        fn new(bytes: Vec<u8>, mdat_range: std::ops::Range<u64>) -> Self {
            Self {
                inner: Cursor::new(bytes),
                mdat_range,
                bytes_read: 0,
                seeks: Vec::new(),
            }
        }
    }

    impl Read for TrackingCursor {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let position = self.inner.position();
            if position >= self.mdat_range.start && position < self.mdat_range.end {
                return Err(std::io::Error::other(format!(
                    "reader touched skipped mdat payload at {position}"
                )));
            }
            let read = self.inner.read(buf)?;
            self.bytes_read += read;
            Ok(read)
        }
    }

    impl Seek for TrackingCursor {
        fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
            let next = self.inner.seek(pos)?;
            self.seeks.push(next);
            Ok(next)
        }
    }

    #[test]
    fn media_track_counts_reader_skips_top_level_mdat_and_reads_only_headers_plus_moov() {
        let fixture = clipline_two_audio_fixture();
        let top = walk(&fixture);
        let mdat = find(&top, b"mdat").expect("fixture has top-level mdat");
        let moov = find(&top, b"moov").expect("fixture has finalized moov");
        let moov_size = usize::try_from(moov.size).unwrap();
        let mut reader =
            TrackingCursor::new(fixture, mdat.payload_offset..(mdat.offset + mdat.size));

        let counts = media_track_counts_reader(&mut reader).unwrap();

        assert_eq!(counts, MediaTrackCounts { video: 1, audio: 2 });
        assert!(
            reader
                .seeks
                .iter()
                .any(|offset| *offset >= mdat.offset + mdat.size),
            "expected seek past mdat payload, got {:?}",
            reader.seeks
        );
        assert!(
            reader.bytes_read <= moov_size + 128,
            "expected bounded reads, got {} bytes for moov size {moov_size}",
            reader.bytes_read
        );
    }

    #[test]
    fn movie_reader_skips_mdat_payload_while_recovering_sample_offsets() {
        let fixture = clipline_two_audio_fixture();
        let top = walk(&fixture);
        let mdat = find(&top, b"mdat").expect("fixture has top-level mdat");
        let moov = find(&top, b"moov").expect("fixture has finalized moov");
        let moov_size = usize::try_from(moov.size).unwrap();
        let mut reader =
            TrackingCursor::new(fixture, mdat.payload_offset..(mdat.offset + mdat.size));

        let movie = parse_movie_reader(&mut reader).unwrap();

        assert_eq!(movie.tracks.len(), 3);
        let first_offset = movie.tracks[0].samples[0].offset as u64;
        assert!(
            first_offset >= mdat.payload_offset && first_offset < mdat.offset + mdat.size,
            "sample offset {first_offset} should remain inside the source mdat"
        );
        assert!(reader.bytes_read <= moov_size + 128);
    }

    #[test]
    fn media_track_counts_file_reports_audio_only_fixture() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "clipline-track-count-file-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("audio-only.mp4");
        std::fs::write(&path, clipline_audio_only_fixture()).unwrap();

        let counts = media_track_counts_file(&path).unwrap();

        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(counts, MediaTrackCounts { video: 0, audio: 1 });
    }

    #[test]
    fn media_track_counts_reader_rejects_oversized_declared_moov_before_allocation() {
        struct LargeMoovReader {
            pos: u64,
            len: u64,
        }

        impl Read for LargeMoovReader {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                const HEADER: [u8; 8] = [0, 0, 0, 0, b'm', b'o', b'o', b'v'];
                if self.pos >= self.len {
                    return Ok(0);
                }
                let mut read = 0usize;
                while read < buf.len() && self.pos < self.len {
                    buf[read] = if self.pos < HEADER.len() as u64 {
                        HEADER[self.pos as usize]
                    } else {
                        0
                    };
                    self.pos += 1;
                    read += 1;
                }
                Ok(read)
            }
        }

        impl Seek for LargeMoovReader {
            fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
                let next = match pos {
                    SeekFrom::Start(offset) => offset,
                    SeekFrom::End(offset) => self
                        .len
                        .checked_add_signed(offset)
                        .ok_or_else(|| std::io::Error::other("seek overflow"))?,
                    SeekFrom::Current(offset) => self
                        .pos
                        .checked_add_signed(offset)
                        .ok_or_else(|| std::io::Error::other("seek overflow"))?,
                };
                self.pos = next;
                Ok(self.pos)
            }
        }

        let mut reader = LargeMoovReader {
            pos: 0,
            len: MAX_FINALIZED_MOOV_BYTES + 1,
        };

        let err = media_track_counts_reader(&mut reader).unwrap_err();

        assert!(
            err.to_string().contains("moov box is too large to inspect"),
            "{err}"
        );
    }

    #[test]
    fn read_top_level_box_supports_extended_size_boxes() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&1u32.to_be_bytes());
        bytes.extend_from_slice(b"moov");
        bytes.extend_from_slice(&24u64.to_be_bytes());
        bytes.extend_from_slice(&[0u8; 8]);
        let limit = bytes.len() as u64;
        let mut reader = Cursor::new(bytes);

        let b = read_top_level_box(&mut reader, 0, limit).unwrap();

        assert_eq!(b.fourcc, *b"moov");
        assert_eq!(b.size, 24);
        assert_eq!(b.payload_offset, 16);
    }

    #[test]
    fn read_top_level_box_treats_size_zero_as_terminal_box() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&8u32.to_be_bytes());
        bytes.extend_from_slice(b"ftyp");
        bytes.extend_from_slice(&0u32.to_be_bytes());
        bytes.extend_from_slice(b"mdat");
        bytes.extend_from_slice(&[0u8; 5]);
        let bytes_len = bytes.len();
        let limit = bytes.len() as u64;
        let mut reader = Cursor::new(bytes);

        let b = read_top_level_box(&mut reader, 8, limit).unwrap();

        assert_eq!(b.fourcc, *b"mdat");
        assert_eq!(b.size, (bytes_len - 8) as u64);
        assert_eq!(b.payload_offset, 16);
    }

    #[test]
    fn read_top_level_box_rejects_truncated_header() {
        let mut reader = Cursor::new(vec![0, 0, 0, 8, b'm', b'o', b'o']);

        let err = read_top_level_box(&mut reader, 0, 7).unwrap_err();

        assert!(err.to_string().contains("truncated box header"), "{err}");
    }

    #[test]
    fn read_top_level_box_rejects_truncated_extended_header() {
        let mut reader = Cursor::new({
            let mut bytes = Vec::new();
            bytes.extend_from_slice(&1u32.to_be_bytes());
            bytes.extend_from_slice(b"moov");
            bytes.extend_from_slice(&[0u8; 4]);
            bytes
        });

        let err = read_top_level_box(&mut reader, 0, 12).unwrap_err();

        assert!(
            err.to_string().contains("truncated largesize box header"),
            "{err}"
        );
    }

    #[test]
    fn read_box_bytes_rejects_truncated_payload() {
        let mut reader = Cursor::new(vec![0u8; 12]);
        let b = BoxInfo {
            fourcc: *b"moov",
            offset: 0,
            size: 16,
            payload_offset: 8,
        };

        let err = read_box_bytes(&mut reader, &b).unwrap_err();

        assert!(matches!(err, TrimError::Io(_)), "{err}");
    }

    #[test]
    fn read_top_level_box_rejects_too_small_box_size() {
        let mut reader = Cursor::new({
            let mut bytes = Vec::new();
            bytes.extend_from_slice(&4u32.to_be_bytes());
            bytes.extend_from_slice(b"moov");
            bytes
        });

        let err = read_top_level_box(&mut reader, 0, 8).unwrap_err();

        assert!(err.to_string().contains("invalid moov box size"), "{err}");
    }

    #[test]
    fn read_top_level_box_rejects_box_extent_past_file() {
        let mut reader = Cursor::new({
            let mut bytes = Vec::new();
            bytes.extend_from_slice(&32u32.to_be_bytes());
            bytes.extend_from_slice(b"moov");
            bytes.extend_from_slice(&[0u8; 8]);
            bytes
        });

        let err = read_top_level_box(&mut reader, 0, 16).unwrap_err();

        assert!(err.to_string().contains("invalid moov box size"), "{err}");
    }

    #[test]
    fn read_finalized_moov_bytes_reports_missing_moov_at_eof_without_looping() {
        let mut reader = Cursor::new({
            let mut bytes = Vec::new();
            bytes.extend_from_slice(&8u32.to_be_bytes());
            bytes.extend_from_slice(b"ftyp");
            bytes.extend_from_slice(&0u32.to_be_bytes());
            bytes.extend_from_slice(b"mdat");
            bytes.extend_from_slice(&[0u8; 3]);
            bytes
        });

        let err = read_finalized_moov_bytes(&mut reader).unwrap_err();

        assert!(err.to_string().contains("missing finalized moov"), "{err}");
    }

    #[test]
    fn remux_with_mixed_audio_track_replaces_selected_tracks_with_one_audible_track() {
        let input = clipline_two_real_opus_audio_fixture();

        let out = remux_with_mixed_audio_track(&input, &[0, 1]).unwrap();
        let movie = parse_movie(&out).unwrap();

        assert_eq!(movie.tracks.len(), 2, "video plus one mixed audio track");
        assert!(matches!(movie.tracks[0].cfg, TrackConfig::Video(_)));
        assert!(matches!(movie.tracks[1].cfg, TrackConfig::Audio(_)));
        assert!(out.windows(6).any(|w| w == b"V00000"));
        assert!(
            decoded_audible_audio_rms(&out) > 0.10,
            "mixed output should decode to audible PCM"
        );
    }

    #[test]
    fn file_audio_mix_streams_video_and_emits_audible_mixed_track() {
        let input = clipline_two_real_opus_audio_fixture();
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("clipline-mix-file-{}-{unique}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("source.mp4");
        let target = dir.join("target.mp4");
        std::fs::write(&source, input).unwrap();

        remux_with_mixed_audio_track_file(&source, &target, &[0, 1]).unwrap();
        let out = std::fs::read(&target).unwrap();
        let movie = parse_movie(&out).unwrap();
        let leftovers = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains("clipline-tmp"))
            .collect::<Vec<_>>();
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(movie.tracks.len(), 2);
        assert!(decoded_audible_audio_rms(&out) > 0.10);
        assert!(leftovers.is_empty(), "leftover temp files: {leftovers:?}");
    }

    #[test]
    fn file_audio_mix_handles_staggered_track_starts() {
        let input = clipline_staggered_opus_audio_fixture();
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "clipline-staggered-mix-file-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("source.mp4");
        let target = dir.join("target.mp4");
        std::fs::write(&source, input).unwrap();

        remux_with_mixed_audio_track_file(&source, &target, &[0, 1]).unwrap();
        let out = std::fs::read(&target).unwrap();
        let movie = parse_movie(&out).unwrap();
        let audio = movie
            .tracks
            .iter()
            .find(|track| matches!(track.cfg, TrackConfig::Audio(_)))
            .unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        assert!(audio.samples.windows(2).all(|samples| {
            samples[1].start_ticks >= samples[0].start_ticks + u64::from(samples[0].duration)
        }));
        assert!(decoded_audible_audio_rms(&out) > 0.02);
    }

    #[test]
    fn audio_mix_preserves_long_gaps_without_encoding_silence_packets() {
        let input = clipline_gapped_opus_audio_fixture();

        let out = remux_with_mixed_audio_track(&input, &[0, 1]).unwrap();
        let movie = parse_movie(&out).unwrap();
        let audio = movie
            .tracks
            .iter()
            .find(|track| matches!(track.cfg, TrackConfig::Audio(_)))
            .unwrap();

        assert_eq!(audio.samples.len(), 2);
        assert_eq!(audio.samples[0].start_ticks, 0);
        assert_eq!(audio.samples[1].start_ticks, 480_000 - 312);
    }

    #[test]
    fn mixed_audio_track_uses_only_the_new_encoder_pre_skip() {
        let input = clipline_two_real_opus_audio_fixture();

        let out = remux_with_mixed_audio_track(&input, &[0, 1]).unwrap();
        let mixed = first_audio_config(&out);
        let encoder = Encoder::new(EncoderConfig::new(48_000, 2)).unwrap();

        assert_eq!(mixed.pre_skip, encoder.get_lookahead().unwrap());
    }

    #[test]
    fn decoded_audio_consumes_per_track_pre_skip_and_quantized_durations() {
        let mut state = AudioMixTrackState {
            next_sample: 0,
            pending: None,
            previous_source_end: None,
            output_cursor: None,
            remaining_pre_skip: 312,
        };
        let first = SampleRecord {
            offset: 0,
            size: 0,
            duration: 959,
            is_sync: true,
            start_ticks: 480,
        };
        let first_pcm = (0..960)
            .flat_map(|frame| [frame as f32, frame as f32])
            .collect();

        let first = prepare_decoded_audio_sample(&first, first_pcm, &mut state)
            .unwrap()
            .unwrap();

        assert_eq!(first.start_ticks, 480);
        assert_eq!(first.pcm.len(), (959 - 312) * 2);
        assert_eq!(first.pcm[0], 312.0);

        let second = SampleRecord {
            offset: 0,
            size: 0,
            duration: 961,
            is_sync: true,
            start_ticks: 1_439,
        };
        let second_pcm = (0..960)
            .flat_map(|frame| [frame as f32 + 1_000.0, frame as f32 + 1_000.0])
            .collect();

        let second = prepare_decoded_audio_sample(&second, second_pcm, &mut state)
            .unwrap()
            .unwrap();

        assert_eq!(second.start_ticks, 1_127);
        assert_eq!(second.pcm.len(), 961 * 2);
        assert_eq!(second.pcm[0], 1_000.0);
        assert_eq!(&second.pcm[second.pcm.len() - 2..], &[0.0, 0.0]);

        let oversized = SampleRecord {
            offset: 0,
            size: 0,
            duration: MAX_OPUS_FRAME_TICKS + 1,
            is_sync: true,
            start_ticks: 0,
        };
        let mut oversized_state = AudioMixTrackState {
            next_sample: 0,
            pending: None,
            previous_source_end: None,
            output_cursor: None,
            remaining_pre_skip: 0,
        };
        assert!(prepare_decoded_audio_sample(
            &oversized,
            vec![0.0; OPUS_MIX_FRAME_TICKS as usize * 2],
            &mut oversized_state,
        )
        .is_err());

        let mismatched = SampleRecord {
            offset: 0,
            size: 0,
            duration: 480,
            is_sync: true,
            start_ticks: 0,
        };
        let mut mismatched_state = AudioMixTrackState {
            next_sample: 0,
            pending: None,
            previous_source_end: None,
            output_cursor: None,
            remaining_pre_skip: 0,
        };
        assert!(prepare_decoded_audio_sample(
            &mismatched,
            vec![0.0; OPUS_MIX_FRAME_TICKS as usize * 2],
            &mut mismatched_state,
        )
        .is_err());
    }

    #[test]
    fn audio_mix_averages_overlapping_tracks_to_avoid_hard_clipping() {
        let mut mixed = vec![1.30, 1.30];
        normalize_mixed_pcm(&mut mixed, &[2]).unwrap();

        assert_eq!(mixed, vec![0.65, 0.65]);
    }

    #[test]
    fn remux_with_mixed_audio_track_rejects_invalid_selection() {
        let input = clipline_two_real_opus_audio_fixture();

        let err = remux_with_mixed_audio_track(&input, &[2]).unwrap_err();

        assert!(
            err.to_string()
                .contains("outside the clip's 2 audio tracks"),
            "{err}"
        );
    }
}
