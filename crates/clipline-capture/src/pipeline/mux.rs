use std::io;
use std::io::{Seek, Write};
use clipline_buffer::{DiskSegment, DiskTrackRef, SampleInfo, Segment};
use clipline_mp4::{AudioTrackConfig, FragSampleRef, HybridMp4Writer, SourceSample, VideoTrackConfig};


#[derive(Clone, Copy, Debug)]
pub(crate) struct SampleSelection {
    pub(crate) first_sample: usize,
    pub(crate) first_byte: usize,
    pub(crate) pts_start_s: Option<f64>,
}

pub(crate) fn select_audio_after_replay_origin(
    pts_start_s: Option<f64>,
    samples: &[SampleInfo],
    payload_len: usize,
    timeline_start_s: f64,
) -> io::Result<SampleSelection> {
    if samples.is_empty() {
        return Ok(SampleSelection {
            first_sample: 0,
            first_byte: 0,
            pts_start_s: None,
        });
    }
    let Some(mut sample_start_s) = pts_start_s else {
        return Ok(SampleSelection {
            first_sample: 0,
            first_byte: 0,
            pts_start_s: None,
        });
    };
    let mut first_sample = 0usize;
    let mut first_byte = 0usize;
    while sample_start_s < timeline_start_s - 1e-9 {
        let Some(sample) = samples.get(first_sample) else {
            break;
        };
        if !sample.duration_s.is_finite() || sample.duration_s < 0.0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid media sample duration",
            ));
        }
        first_byte = first_byte
            .checked_add(sample.size as usize)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "sample byte range overflow")
            })?;
        sample_start_s += sample.duration_s;
        if !sample_start_s.is_finite() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid media sample timestamp",
            ));
        }
        first_sample += 1;
    }
    if first_byte > payload_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "sample metadata exceeds declared track data",
        ));
    }
    Ok(SampleSelection {
        first_sample,
        first_byte,
        pts_start_s: (first_sample < samples.len()).then_some(sample_start_s),
    })
}

pub(crate) fn segment_audio_selections(
    segment: &Segment,
    replay_origin_s: Option<f64>,
) -> io::Result<Vec<SampleSelection>> {
    segment
        .audio
        .iter()
        .map(|track| {
            if let Some(origin_s) = replay_origin_s {
                select_audio_after_replay_origin(
                    track.pts_start_s,
                    &track.samples,
                    track.data.len(),
                    origin_s,
                )
            } else {
                Ok(SampleSelection {
                    first_sample: 0,
                    first_byte: 0,
                    pts_start_s: track.pts_start_s,
                })
            }
        })
        .collect()
}

pub(crate) fn write_memory_replay_segment<W: Write + Seek>(
    writer: &mut HybridMp4Writer<W>,
    segment: &Segment,
    video_cfg: &VideoTrackConfig,
    audio_cfgs: &[AudioTrackConfig],
    timeline_origin_s: f64,
) -> io::Result<()> {
    let audio_selections = segment_audio_selections(segment, Some(timeline_origin_s))?;
    let timelines = set_segment_decode_times(
        writer,
        segment.pts_start_s,
        &audio_selections,
        video_cfg,
        audio_cfgs,
        timeline_origin_s,
    )?;
    let per_track = segment_fragment_refs(
        segment,
        &audio_selections,
        video_cfg,
        audio_cfgs,
        &timelines,
    )?;
    let slices: Vec<&[FragSampleRef<'_>]> = per_track.iter().map(Vec::as_slice).collect();
    writer.write_fragment_multi_borrowed(&slices)
}

pub(crate) fn write_disk_replay_segment<W: Write + Seek>(
    writer: &mut HybridMp4Writer<W>,
    segment: &DiskSegment,
    video_cfg: &VideoTrackConfig,
    audio_cfgs: &[AudioTrackConfig],
    timeline_origin_s: f64,
) -> io::Result<()> {
    let audio_tracks: Vec<_> = segment.audio_tracks().collect();
    let audio_selections: Vec<_> = audio_tracks
        .iter()
        .map(|track| {
            select_audio_after_replay_origin(
                track.pts_start_s,
                track.samples,
                track.byte_len,
                timeline_origin_s,
            )
        })
        .collect::<io::Result<_>>()?;
    let timelines = set_segment_decode_times(
        writer,
        segment.pts_start_s,
        &audio_selections,
        video_cfg,
        audio_cfgs,
        timeline_origin_s,
    )?;
    let video = segment.video_track();
    let video_timeline = timelines.first().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "video fragment timeline is missing",
        )
    })?;
    let mut per_track = vec![quantized_source_samples(
        video,
        SampleSelection {
            first_sample: 0,
            first_byte: 0,
            pts_start_s: video.pts_start_s,
        },
        video_cfg.timescale,
        *video_timeline,
    )?];
    for (index, ((track, selection), cfg)) in audio_tracks
        .iter()
        .zip(&audio_selections)
        .zip(audio_cfgs)
        .enumerate()
    {
        let timeline = timelines.get(index + 1).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "audio fragment timeline is missing",
            )
        })?;
        per_track.push(quantized_source_samples(
            *track,
            *selection,
            cfg.sample_rate,
            *timeline,
        )?);
    }
    per_track.resize_with(1 + audio_cfgs.len(), Vec::new);
    let slices: Vec<&[SourceSample]> = per_track.iter().map(Vec::as_slice).collect();
    let mut source = segment.open_payload()?;
    writer.write_fragment_multi_from_source(&mut source, &slices)
}

#[derive(Clone, Copy)]
pub(crate) struct FragmentTimeline {
    pub(crate) requested_start: u64,
    pub(crate) write_start: u64,
}

pub(crate) fn segment_fragment_refs<'a>(
    seg: &'a Segment,
    audio_selections: &[SampleSelection],
    video_cfg: &VideoTrackConfig,
    audio_cfgs: &[AudioTrackConfig],
    timelines: &[FragmentTimeline],
) -> io::Result<Vec<Vec<FragSampleRef<'a>>>> {
    let video_timeline = timelines.first().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "video fragment timeline is missing",
        )
    })?;
    let video = quantized_fragment_refs(
        seg.sample_slices(),
        &seg.samples,
        video_cfg.timescale,
        *video_timeline,
    )?;
    let mut per_track: Vec<Vec<FragSampleRef<'a>>> = vec![video];
    for (index, (track, cfg)) in seg.audio.iter().zip(audio_cfgs).enumerate() {
        let selection = audio_selections.get(index).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "audio sample selection is missing",
            )
        })?;
        let samples = track.samples.get(selection.first_sample..).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "audio sample selection exceeds track metadata",
            )
        })?;
        let timeline = timelines.get(index + 1).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "audio fragment timeline is missing",
            )
        })?;
        per_track.push(quantized_fragment_refs(
            track.sample_slices().skip(selection.first_sample),
            samples,
            cfg.sample_rate,
            *timeline,
        )?);
    }
    // Segments recorded before an audio source was attached have fewer audio
    // tracks; pad with empty runs to keep alignment.
    per_track.resize_with(1 + audio_cfgs.len(), Vec::new);
    Ok(per_track)
}

fn quantized_fragment_refs<'a>(
    slices: impl Iterator<Item = io::Result<&'a [u8]>>,
    samples: &[SampleInfo],
    timescale: u32,
    timeline: FragmentTimeline,
) -> io::Result<Vec<FragSampleRef<'a>>> {
    let durations = quantized_sample_durations(samples, timescale, timeline)?;
    slices
        .zip(samples)
        .zip(durations)
        .map(|((slice, info), duration)| {
            Ok(FragSampleRef {
                data: slice?,
                duration,
                is_sync: info.is_sync,
            })
        })
        .collect()
}

fn quantized_source_samples(
    track: DiskTrackRef<'_>,
    selection: SampleSelection,
    timescale: u32,
    timeline: FragmentTimeline,
) -> io::Result<Vec<SourceSample>> {
    if selection.first_byte > track.byte_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "sample metadata exceeds declared track data",
        ));
    }
    let samples = track.samples.get(selection.first_sample..).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "sample selection exceeds track metadata",
        )
    })?;
    let durations = quantized_sample_durations(samples, timescale, timeline)?;
    let track_end = track
        .offset
        .checked_add(track.byte_len as u64)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "track byte range overflow"))?;
    let mut offset = track
        .offset
        .checked_add(selection.first_byte as u64)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "track byte range overflow"))?;
    samples
        .iter()
        .zip(durations)
        .map(|(info, duration)| {
            let sample_offset = offset;
            offset = offset.checked_add(u64::from(info.size)).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "sample byte range overflow")
            })?;
            if offset > track_end {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "sample metadata exceeds declared track data",
                ));
            }
            Ok(SourceSample {
                offset: sample_offset,
                size: info.size,
                duration,
                is_sync: info.is_sync,
            })
        })
        .collect()
}

fn quantized_sample_durations(
    samples: &[SampleInfo],
    timescale: u32,
    timeline: FragmentTimeline,
) -> io::Result<Vec<u32>> {
    let scale = f64::from(timescale);
    let total_s = samples.iter().try_fold(0.0_f64, |total, sample| {
        let next = total + sample.duration_s;
        if next.is_finite() && next >= 0.0 {
            Ok(next)
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid media sample duration",
            ))
        }
    })?;
    let relative_total_ticks = total_s * scale;
    if !relative_total_ticks.is_finite() || relative_total_ticks > u64::MAX as f64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "media duration overflow",
        ));
    }
    let requested_end = timeline
        .requested_start
        .checked_add(relative_total_ticks.round() as u64)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "media duration overflow"))?;
    let sample_count = u64::try_from(samples.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "media sample count overflow"))?;
    let minimum_end = timeline
        .write_start
        .checked_add(sample_count)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "media duration overflow"))?;
    let target_end = requested_end.max(minimum_end);

    let mut elapsed_s = 0.0_f64;
    let mut previous_end = timeline.write_start;
    samples
        .iter()
        .enumerate()
        .map(|(index, info)| {
            elapsed_s += info.duration_s;
            let relative_end = elapsed_s * scale;
            if !relative_end.is_finite() || relative_end < 0.0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid media sample duration",
                ));
            }
            // Quantize against the requested absolute timeline, but allocate
            // from the writer's actual frontier. A prior rounded overlap is
            // therefore absorbed by this run instead of becoming permanent
            // drift. Per-segment accumulation keeps the f64 error far below
            // half a timescale tick before this rounding step.
            let desired_end = timeline
                .requested_start
                .checked_add(relative_end.round() as u64)
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "media duration overflow")
                })?;
            let remaining = sample_count - index as u64 - 1;
            let earliest_end = previous_end + 1;
            let latest_end = target_end - remaining;
            let end_ticks = desired_end.clamp(earliest_end, latest_end);
            let duration = end_ticks - previous_end;
            previous_end = end_ticks;
            u32::try_from(duration).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "media sample duration exceeds MP4 field",
                )
            })
        })
        .collect()
}

pub(crate) fn set_segment_decode_times<W: Write + Seek>(
    writer: &mut HybridMp4Writer<W>,
    segment_pts_start_s: f64,
    audio_selections: &[SampleSelection],
    video_cfg: &VideoTrackConfig,
    audio_cfgs: &[AudioTrackConfig],
    timeline_origin_s: f64,
) -> io::Result<Vec<FragmentTimeline>> {
    let mut timelines = vec![advance_track_decode_time(
        writer,
        0,
        relative_pts_ticks(segment_pts_start_s, timeline_origin_s, video_cfg.timescale)?,
    )?];
    for (index, cfg) in audio_cfgs.iter().enumerate() {
        let requested = audio_selections
            .get(index)
            .and_then(|selection| selection.pts_start_s)
            .map(|start_s| relative_pts_ticks(start_s, timeline_origin_s, cfg.sample_rate))
            .transpose()?;
        let timeline = if let Some(requested) = requested {
            advance_track_decode_time(writer, index + 1, requested)?
        } else {
            let current = writer.track_decode_time(index + 1)?;
            FragmentTimeline {
                requested_start: current,
                write_start: current,
            }
        };
        timelines.push(timeline);
    }
    Ok(timelines)
}

pub(crate) fn advance_track_decode_time<W: Write + Seek>(
    writer: &mut HybridMp4Writer<W>,
    track_index: usize,
    requested: u64,
) -> io::Result<FragmentTimeline> {
    let current = writer.track_decode_time(track_index)?;
    if requested > current {
        writer.set_track_decode_time(track_index, requested)?;
    }
    Ok(FragmentTimeline {
        requested_start: requested,
        write_start: current.max(requested),
    })
}

fn relative_pts_ticks(pts_s: f64, origin_s: f64, timescale: u32) -> io::Result<u64> {
    let relative = pts_s - origin_s;
    if !relative.is_finite() || relative < -1e-9 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "media sample timestamp precedes recording origin",
        ));
    }
    let ticks = relative.max(0.0) * f64::from(timescale);
    if ticks > u64::MAX as f64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "media sample timestamp exceeds MP4 timeline",
        ));
    }
    Ok(ticks.round() as u64)
}
