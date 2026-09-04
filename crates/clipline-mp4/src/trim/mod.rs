//! Keyframe-aligned stream-copy trim for finalized Clipline MP4s.

mod audio_mix;
mod files;
mod model;
mod parse;
mod tables;

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{Cursor, Read, Seek, Write};
use std::ops::Range;
use std::path::Path;

use crate::{FragSample, HybridMp4Writer, SourceSample, TrackConfig};
use self::audio_mix::mix_selected_opus_audio_tracks;
use self::model::{ParsedMovie, ParsedTrack, SampleRecord, select_trim_range, validate_range};
use self::parse::{
    finalized_movie_track_counts, finalized_movie_video_codecs, media_track_counts_reader,
    parse_movie,
};
use self::tables::read_finalized_moov_bytes;

pub use self::files::{
    remux_with_mixed_audio_track_file, remux_with_selected_audio_tracks_file,
    trim_keyframe_aligned_file,
};
pub use self::model::{MediaTrackCounts, MediaVideoCodec, TrimError, TrimInfo};

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

pub(crate) fn selected_audio_index_set(
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

pub(crate) fn selected_audio_tracks<'a>(
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

pub(crate) fn write_timed_source_samples<R: Read + Seek, W: Write + Seek>(
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

pub(crate) fn write_timed_source_samples_from_sources<W: Write + Seek>(
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

#[cfg(test)]
mod tests {
    use super::model::fixtures::*;
    use super::parse::parse_movie;
    use super::*;
    use crate::VideoCodecParams;

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
}
