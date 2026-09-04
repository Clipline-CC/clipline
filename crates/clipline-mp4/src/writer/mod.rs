use std::io::{self, Read, Seek, SeekFrom, Write};

use crate::boxes::{mp4_box, Payload};
use crate::fragment::{
    fragment_moof_multi, mdat_header, FragSample, FragSampleInfo, FragSampleRef, FragmentError,
    TrackRunInfo,
};
use crate::init::{free_placeholder, ftyp, moov_init_multi, mvhd, TrackConfig, VideoTrackConfig};

mod track_state;

use self::track_state::TrackState;

/// Streaming Hybrid MP4 writer (ddoc §10). While recording the file is a
/// fragmented MP4 (crash-safe); `finalize()` turns it into a standard
/// seekable MP4 in place. Supports N tracks (video + audio).
pub struct HybridMp4Writer<W: Write + Seek> {
    w: W,
    tracks: Vec<TrackState>,
    free_offset: u64,
    next_sequence: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct SourceSample {
    pub offset: u64,
    pub size: u32,
    pub duration: u32,
    pub is_sync: bool,
}

trait FragmentSampleMeta {
    fn fragment_info(&self) -> io::Result<FragSampleInfo>;
}

impl FragmentSampleMeta for FragSample {
    fn fragment_info(&self) -> io::Result<FragSampleInfo> {
        Ok(FragSampleInfo {
            size: u32::try_from(self.data.len())
                .map_err(|_| fragment_io_error(FragmentError::SampleSizeExceedsU32))?,
            duration: self.duration,
            is_sync: self.is_sync,
        })
    }
}

impl FragmentSampleMeta for FragSampleRef<'_> {
    fn fragment_info(&self) -> io::Result<FragSampleInfo> {
        Ok(FragSampleInfo {
            size: u32::try_from(self.data.len())
                .map_err(|_| fragment_io_error(FragmentError::SampleSizeExceedsU32))?,
            duration: self.duration,
            is_sync: self.is_sync,
        })
    }
}

impl FragmentSampleMeta for SourceSample {
    fn fragment_info(&self) -> io::Result<FragSampleInfo> {
        Ok(FragSampleInfo {
            size: self.size,
            duration: self.duration,
            is_sync: self.is_sync,
        })
    }
}

/// Seekable sample source used when one output contains media from more than
/// one backing file (for example source video plus a spooled mixed-audio track).
pub trait ReadSeek: Read + Seek {}

impl<T: Read + Seek + ?Sized> ReadSeek for T {}

impl<W: Write + Seek> HybridMp4Writer<W> {
    /// Single video track (original API).
    pub fn new(w: W, cfg: VideoTrackConfig) -> io::Result<Self> {
        Self::new_multi(w, vec![TrackConfig::Video(cfg)])
    }

    pub fn new_multi(mut w: W, tracks: Vec<TrackConfig>) -> io::Result<Self> {
        validate_track_configs(&tracks)?;
        let ftyp = ftyp();
        w.write_all(&ftyp)?;
        let free_offset = ftyp.len() as u64;
        w.write_all(&free_placeholder())?;
        w.write_all(&moov_init_multi(&tracks))?;
        Ok(Self {
            w,
            tracks: tracks
                .into_iter()
                .map(TrackState::new)
                .collect(),
            free_offset,
            next_sequence: 1,
        })
    }

    /// Single-track fragment write (original API; requires exactly 1 track).
    pub fn write_fragment(&mut self, samples: &[FragSample]) -> io::Result<()> {
        self.write_fragment_multi(&[samples])
    }

    /// Advance one track to an absolute decode timestamp in its own
    /// timescale. The next non-empty fragment for that track begins there.
    /// Internal video gaps shorter than the preceding sample are absorbed
    /// into that sample; leading, frame-sized, and audio gaps remain exact.
    pub fn set_track_decode_time(
        &mut self,
        track_index: usize,
        decode_time: u64,
    ) -> io::Result<()> {
        let state = self
            .tracks
            .get_mut(track_index)
            .ok_or_else(|| invalid_config(format!("track index {track_index} is out of range")))?;
        if decode_time < state.next_decode_time {
            return Err(invalid_config(format!(
                "track {track_index} decode time cannot move backward from {} to {decode_time}",
                state.next_decode_time
            )));
        }
        if state.absorb_subframe_video_gap(decode_time)? {
            return Ok(());
        }
        state.next_decode_time = decode_time;
        Ok(())
    }

    /// Return the decode timestamp where this track's next sample will begin.
    pub fn track_decode_time(&self, track_index: usize) -> io::Result<u64> {
        self.tracks
            .get(track_index)
            .map(|state| state.next_decode_time)
            .ok_or_else(|| invalid_config(format!("track index {track_index} is out of range")))
    }

    /// One fragment carrying samples for each track, positionally aligned
    /// with the track list. Empty slices are allowed (track sat this
    /// fragment out).
    pub fn write_fragment_multi(&mut self, per_track: &[&[FragSample]]) -> io::Result<()> {
        let Some(info_storage) = fragment_info_storage(per_track, self.tracks.len())? else {
            return Ok(());
        };
        self.write_planned_fragment(&info_storage, |writer, track, sample| {
            writer.write_all(&per_track[track][sample].data)
        })
    }

    /// Borrowed variant for already-contiguous GOP buffers. This avoids
    /// allocating one `Vec<u8>` per sample before immediately writing it out.
    pub fn write_fragment_multi_borrowed(
        &mut self,
        per_track: &[&[FragSampleRef<'_>]],
    ) -> io::Result<()> {
        let Some(info_storage) = fragment_info_storage(per_track, self.tracks.len())? else {
            return Ok(());
        };
        self.write_planned_fragment(&info_storage, |writer, track, sample| {
            writer.write_all(per_track[track][sample].data)
        })
    }

    pub fn write_fragment_multi_from_source<R: Read + Seek>(
        &mut self,
        source: &mut R,
        per_track: &[&[SourceSample]],
    ) -> io::Result<()> {
        let mut copy_buf = vec![0u8; 64 * 1024];
        let Some(info_storage) = fragment_info_storage(per_track, self.tracks.len())? else {
            return Ok(());
        };
        self.write_planned_fragment(&info_storage, |writer, track, sample| {
            let sample = &per_track[track][sample];
            source.seek(SeekFrom::Start(sample.offset))?;
            copy_exact(source, writer, u64::from(sample.size), &mut copy_buf)
        })
    }

    pub fn write_fragment_multi_from_sources(
        &mut self,
        sources: &mut [&mut dyn ReadSeek],
        per_track: &[&[SourceSample]],
    ) -> io::Result<()> {
        if per_track.len() != self.tracks.len() || sources.len() != self.tracks.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "expected {} track sources and slices, got {} sources and {} slices",
                    self.tracks.len(),
                    sources.len(),
                    per_track.len()
                ),
            ));
        }
        let mut copy_buf = vec![0_u8; 64 * 1024];
        let Some(info_storage) = fragment_info_storage(per_track, self.tracks.len())? else {
            return Ok(());
        };
        self.write_planned_fragment(&info_storage, |writer, track, sample| {
            let sample = &per_track[track][sample];
            sources[track].seek(SeekFrom::Start(sample.offset))?;
            copy_exact(
                &mut sources[track],
                writer,
                u64::from(sample.size),
                &mut copy_buf,
            )
        })
    }

    fn write_planned_fragment<F>(
        &mut self,
        info_storage: &[Vec<FragSampleInfo>],
        mut write_payload: F,
    ) -> io::Result<()>
    where
        F: FnMut(&mut W, usize, usize) -> io::Result<()>,
    {
        let runs: Vec<TrackRunInfo<'_>> = info_storage
            .iter()
            .enumerate()
            .filter(|(_, samples)| !samples.is_empty())
            .map(|(index, samples)| TrackRunInfo {
                track_id: index as u32 + 1,
                base_decode_time: self.tracks[index].next_decode_time,
                samples,
            })
            .collect();
        let total_payload = info_storage
            .iter()
            .flatten()
            .try_fold(0_u64, |total, sample| {
                total.checked_add(u64::from(sample.size)).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "fragment payload size overflow",
                    )
                })
            })?;

        let frag_start = self.w.stream_position()?;
        let moof = fragment_moof_multi(self.next_sequence, &runs).map_err(fragment_io_error)?;
        let mdat_header = mdat_header(total_payload);
        self.w.write_all(&moof)?;
        self.w.write_all(&mdat_header)?;
        for (track_index, samples) in info_storage.iter().enumerate() {
            for sample_index in 0..samples.len() {
                write_payload(&mut self.w, track_index, sample_index)?;
            }
        }

        let mut sample_offset = frag_start + moof.len() as u64 + mdat_header.len() as u64;
        for (track_index, samples) in info_storage.iter().enumerate() {
            if samples.is_empty() {
                continue;
            }
            let state = &mut self.tracks[track_index];
            state.record_run(samples.iter().map(|sample| sample.duration))?;
            state.record_chunk(sample_offset, samples.len())?;
            for sample in samples {
                state.record_sample(sample)?;
                sample_offset += u64::from(sample.size);
            }
        }
        self.next_sequence += 1;
        Ok(())
    }

    /// Append the full moov, then overwrite the leading free box with a
    /// largesize mdat header spanning init-moov + all fragments — hiding
    /// them so the file parses as ftyp / mdat / moov (ddoc §10).
    pub fn finalize(mut self) -> io::Result<W> {
        let moov_offset = self.w.stream_position()?;
        let moov = self.final_moov();
        self.w.write_all(&moov)?;

        let hidden_span = moov_offset - self.free_offset;
        self.w.seek(SeekFrom::Start(self.free_offset))?;
        let mut hdr = Payload::new();
        hdr.u32(1).bytes(b"mdat").u64(hidden_span);
        self.w.write_all(&hdr.into_vec())?;
        self.w.flush()?;
        Ok(self.w)
    }

    /// Abort without finalizing (crash-simulation / tests): hand back the
    /// underlying writer with the fragmented layout intact.
    pub fn into_inner(self) -> W {
        self.w
    }

    fn final_moov(&self) -> Vec<u8> {
        let duration_movie = self
            .tracks
            .iter()
            .map(|t| t.duration_movie_ts())
            .max()
            .unwrap_or(0);

        let mut moov = mvhd(duration_movie, self.tracks.len() as u32 + 1);
        for (i, t) in self.tracks.iter().enumerate() {
            moov.extend(t.trak(i as u32 + 1));
        }
        mp4_box(*b"moov", moov)
    }
}

fn fragment_info_storage<S: FragmentSampleMeta>(
    per_track: &[&[S]],
    expected_tracks: usize,
) -> io::Result<Option<Vec<Vec<FragSampleInfo>>>> {
    if per_track.len() != expected_tracks {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "expected {expected_tracks} track slices, got {}",
                per_track.len()
            ),
        ));
    }
    if per_track.iter().all(|samples| samples.is_empty()) {
        return Ok(None);
    }

    let info_storage = per_track
        .iter()
        .map(|samples| {
            samples
                .iter()
                .map(FragmentSampleMeta::fragment_info)
                .collect::<io::Result<Vec<_>>>()
        })
        .collect::<io::Result<Vec<_>>>()?;
    validate_nonzero_durations(info_storage.iter().flatten().map(|sample| sample.duration))?;
    Ok(Some(info_storage))
}

fn validate_track_configs(tracks: &[TrackConfig]) -> io::Result<()> {
    if tracks.is_empty() {
        return Err(invalid_config("at least one MP4 track is required"));
    }
    for (index, track) in tracks.iter().enumerate() {
        match track {
            TrackConfig::Video(cfg) => {
                if cfg.width == 0 || cfg.height == 0 {
                    return Err(invalid_config(format!(
                        "video track {index} dimensions must be nonzero"
                    )));
                }
                if cfg.timescale == 0 {
                    return Err(invalid_config(format!(
                        "video track {index} timescale must be nonzero"
                    )));
                }
                match &cfg.codec {
                    crate::init::VideoCodecParams::H264 { sps, pps } => {
                        validate_parameter_sets(index, "H.264 SPS", sps, 31)?;
                        validate_parameter_sets(index, "H.264 PPS", pps, u8::MAX as usize)?;
                    }
                    crate::init::VideoCodecParams::Hevc { vps, sps, pps } => {
                        validate_parameter_sets(index, "HEVC VPS", vps, u16::MAX as usize)?;
                        validate_parameter_sets(index, "HEVC SPS", sps, u16::MAX as usize)?;
                        validate_parameter_sets(index, "HEVC PPS", pps, u16::MAX as usize)?;
                    }
                    crate::init::VideoCodecParams::Av1 {
                        sequence_header_obu,
                    } if sequence_header_obu.is_empty() => {
                        return Err(invalid_config(format!(
                            "video track {index} AV1 sequence header must be nonempty"
                        )));
                    }
                    crate::init::VideoCodecParams::Av1 { .. } => {}
                }
            }
            TrackConfig::Audio(cfg) => {
                if cfg.channels == 0 || cfg.channels > u8::MAX as u16 {
                    return Err(invalid_config(format!(
                        "audio track {index} channel count must fit dOps"
                    )));
                }
                if cfg.sample_rate == 0 || cfg.sample_rate > u16::MAX as u32 {
                    return Err(invalid_config(format!(
                        "audio track {index} sample rate must fit 16.16 sample-entry rate"
                    )));
                }
            }
        }
    }
    Ok(())
}

fn validate_parameter_sets(
    track_index: usize,
    label: &str,
    sets: &[Vec<u8>],
    max_count: usize,
) -> io::Result<()> {
    if sets.is_empty() || sets.len() > max_count {
        return Err(invalid_config(format!(
            "video track {track_index} {label} count must be 1..={max_count}"
        )));
    }
    if sets
        .iter()
        .any(|set| set.is_empty() || set.len() > u16::MAX as usize)
    {
        return Err(invalid_config(format!(
            "video track {track_index} {label} entries must be 1..=65535 bytes"
        )));
    }
    Ok(())
}

pub(crate) fn invalid_config(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn validate_nonzero_durations(durations: impl Iterator<Item = u32>) -> io::Result<()> {
    if durations.into_iter().any(|duration| duration == 0) {
        Err(invalid_config("MP4 sample duration must be nonzero"))
    } else {
        Ok(())
    }
}

fn copy_exact<R: Read, W: Write>(
    source: &mut R,
    dest: &mut W,
    mut remaining: u64,
    buf: &mut [u8],
) -> io::Result<()> {
    while remaining > 0 {
        let n = remaining.min(buf.len() as u64) as usize;
        source.read_exact(&mut buf[..n])?;
        dest.write_all(&buf[..n])?;
        remaining -= n as u64;
    }
    Ok(())
}

fn fragment_io_error(error: FragmentError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, error)
}

#[cfg(test)]
mod tests {
    use super::track_state::rescale_duration;
    use super::track_state::support::{all_sync_gop, audio_cfg, gop, read_i32_at, read_u32_at, video_cfg};
    use super::*;
    use crate::fragment::FragSample;
    use crate::init::{AudioTrackConfig, EditListEntry, VideoTrackConfig, MOVIE_TIMESCALE};
    use crate::walker::{children, find, walk};
    use std::io::Cursor;

    #[test]
    fn subframe_video_gap_extends_previous_frame_but_frame_gap_remains() {
        let mut subframe = HybridMp4Writer::new(Cursor::new(Vec::new()), video_cfg()).unwrap();
        subframe.write_fragment(&gop(0)).unwrap();
        subframe.set_track_decode_time(0, 9_001).unwrap();
        subframe.write_fragment(&gop(3)).unwrap();

        assert_eq!(subframe.tracks[0].duration_media_ts(), 18_001);
        assert!(subframe.tracks[0].edit_list().is_empty());

        let mut full_frame = HybridMp4Writer::new(Cursor::new(Vec::new()), video_cfg()).unwrap();
        full_frame.write_fragment(&gop(0)).unwrap();
        full_frame.set_track_decode_time(0, 12_000).unwrap();
        full_frame.write_fragment(&gop(3)).unwrap();
        assert_eq!(
            full_frame.tracks[0].edit_list(),
            vec![
                EditListEntry {
                    duration_movie_ts: 72_000,
                    media_time: 0,
                },
                EditListEntry {
                    duration_movie_ts: 24_000,
                    media_time: -1,
                },
                EditListEntry {
                    duration_movie_ts: 72_000,
                    media_time: 9_000,
                },
            ]
        );
    }

    #[test]
    fn explicit_track_decode_times_survive_fragments_and_final_edit_list() {
        let tracks = vec![
            TrackConfig::Video(video_cfg()),
            TrackConfig::Audio(audio_cfg()),
        ];
        let mut writer = HybridMp4Writer::new_multi(Cursor::new(Vec::new()), tracks).unwrap();
        let video = vec![FragSample {
            data: vec![0x01],
            duration: 90_000,
            is_sync: true,
        }];
        let audio = vec![FragSample {
            data: vec![0x02],
            duration: 960,
            is_sync: true,
        }];

        writer.write_fragment_multi(&[&video, &[]]).unwrap();
        writer.set_track_decode_time(1, 48_000).unwrap();
        writer.write_fragment_multi(&[&video, &audio]).unwrap();
        writer.set_track_decode_time(1, 96_000).unwrap();
        writer.write_fragment_multi(&[&video, &audio]).unwrap();
        let bytes = writer.finalize().unwrap().into_inner();

        let tfdt_values: Vec<u64> = bytes
            .windows(4)
            .enumerate()
            .filter(|(_, fourcc)| *fourcc == b"tfdt")
            .map(|(offset, _)| {
                assert_eq!(bytes[offset + 4], 1, "writer emits version-1 tfdt");
                u64::from_be_bytes(bytes[offset + 8..offset + 16].try_into().unwrap())
            })
            .collect();
        assert!(tfdt_values.contains(&48_000));
        assert!(tfdt_values.contains(&96_000));

        let elst_fourcc = bytes
            .windows(4)
            .position(|fourcc| fourcc == b"elst")
            .expect("final track with gaps has elst");
        let payload = elst_fourcc + 4;
        assert_eq!(bytes[payload], 0, "small edit list uses version zero");
        assert_eq!(read_u32_at(&bytes, payload + 4), 4);
        let mut pos = payload + 8;
        let mut entries = Vec::new();
        for _ in 0..4 {
            entries.push((read_u32_at(&bytes, pos), read_i32_at(&bytes, pos + 4)));
            assert_eq!(read_u32_at(&bytes, pos + 8), 0x0001_0000);
            pos += 12;
        }
        assert_eq!(
            entries,
            vec![(720_000, -1), (14_400, 0), (705_600, -1), (14_400, 960)]
        );
    }

    #[test]
    fn explicit_decode_time_rejects_unknown_track_backward_motion_and_zero_duration() {
        let mut writer = HybridMp4Writer::new(Cursor::new(Vec::new()), video_cfg()).unwrap();
        assert_eq!(
            writer.set_track_decode_time(1, 1).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        writer.set_track_decode_time(0, 100).unwrap();
        assert_eq!(writer.track_decode_time(0).unwrap(), 100);
        assert_eq!(
            writer.track_decode_time(1).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            writer.set_track_decode_time(0, 99).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        let zero = [FragSample {
            data: vec![1],
            duration: 0,
            is_sync: true,
        }];
        assert_eq!(
            writer.write_fragment(&zero).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn new_multi_rejects_invalid_public_track_configuration_before_writing() {
        let mut invalid_video = video_cfg();
        invalid_video.timescale = 0;
        let error = match HybridMp4Writer::new(Cursor::new(Vec::new()), invalid_video) {
            Ok(_) => panic!("invalid video config was accepted"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);

        let invalid_audio = AudioTrackConfig {
            channels: 256,
            sample_rate: 48_000,
            pre_skip: 0,
        };
        let error = match HybridMp4Writer::new_multi(
            Cursor::new(Vec::new()),
            vec![TrackConfig::Audio(invalid_audio)],
        ) {
            Ok(_) => panic!("invalid audio config was accepted"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);

        let invalid_params = VideoTrackConfig {
            width: 64,
            height: 64,
            timescale: 90_000,
            codec: crate::init::VideoCodecParams::H264 {
                sps: vec![vec![0; u16::MAX as usize + 1]],
                pps: vec![vec![1]],
            },
        };
        let error = match HybridMp4Writer::new(Cursor::new(Vec::new()), invalid_params) {
            Ok(_) => panic!("overlong parameter set was accepted"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn duration_rescale_uses_wide_intermediate() {
        let duration = u64::MAX;
        assert_eq!(
            rescale_duration(duration, MOVIE_TIMESCALE, MOVIE_TIMESCALE),
            duration
        );
    }

    #[test]
    fn duration_rescale_saturates_only_final_overflow() {
        assert_eq!(rescale_duration(u64::MAX, 1, u32::MAX), u64::MAX);
        assert_eq!(
            rescale_duration(u64::MAX, u32::MAX, 1),
            (u64::MAX as u128 / u32::MAX as u128) as u64
        );
    }

    // --- HybridMp4Writer API tests ---

    #[test]
    fn write_fragment_multi_rejects_track_count_mismatch() {
        let mut w = HybridMp4Writer::new(Cursor::new(Vec::new()), video_cfg()).unwrap();
        let s1 = gop(0);
        let s2 = gop(3);
        let err = w.write_fragment_multi(&[&s1, &s2]);
        assert!(err.is_err(), "2 slices for a 1-track writer");
    }

    #[test]
    fn write_fragment_multi_no_ops_on_all_empty() {
        let mut w = HybridMp4Writer::new(Cursor::new(Vec::new()), video_cfg()).unwrap();
        let before = w.w.position();
        w.write_fragment_multi(&[&[]]).unwrap();
        assert_eq!(w.w.position(), before, "nothing written for empty samples");
    }

    #[test]
    fn write_fragment_multi_borrowed_rejects_track_count_mismatch() {
        let mut w = HybridMp4Writer::new_multi(
            Cursor::new(Vec::new()),
            vec![
                TrackConfig::Video(video_cfg()),
                TrackConfig::Audio(audio_cfg()),
            ],
        )
        .unwrap();
        let samples = gop(0);
        let refs: Vec<FragSampleRef<'_>> = samples
            .iter()
            .map(|s| FragSampleRef {
                data: &s.data,
                duration: s.duration,
                is_sync: s.is_sync,
            })
            .collect();
        let err = w.write_fragment_multi_borrowed(&[&refs]);
        assert!(err.is_err(), "1 slice for a 2-track writer");
    }

    #[test]
    fn write_fragment_multi_borrowed_no_ops_on_all_empty() {
        let mut w = HybridMp4Writer::new_multi(
            Cursor::new(Vec::new()),
            vec![
                TrackConfig::Video(video_cfg()),
                TrackConfig::Audio(audio_cfg()),
            ],
        )
        .unwrap();
        let before = w.w.position();
        let empty_v: &[FragSampleRef<'_>] = &[];
        let empty_a: &[FragSampleRef<'_>] = &[];
        w.write_fragment_multi_borrowed(&[empty_v, empty_a])
            .unwrap();
        assert_eq!(w.w.position(), before);
    }

    #[test]
    fn write_fragment_multi_from_source_rejects_track_count_mismatch() {
        let mut w = HybridMp4Writer::new(Cursor::new(Vec::new()), video_cfg()).unwrap();
        let source_data = vec![0u8; 100];
        let mut source = Cursor::new(source_data);
        let samples = [SourceSample {
            offset: 0,
            size: 10,
            duration: 3000,
            is_sync: true,
        }];
        let err = w.write_fragment_multi_from_source(&mut source, &[&samples, &samples]);
        assert!(err.is_err());
    }

    #[test]
    fn write_fragment_multi_from_source_no_ops_on_all_empty() {
        let mut w = HybridMp4Writer::new(Cursor::new(Vec::new()), video_cfg()).unwrap();
        let mut source = Cursor::new(Vec::<u8>::new());
        let before = w.w.position();
        let empty: &[SourceSample] = &[];
        w.write_fragment_multi_from_source(&mut source, &[empty])
            .unwrap();
        assert_eq!(w.w.position(), before);
    }

    #[test]
    fn fragment_transports_emit_identical_bytes() {
        let owned = gop(7);
        let borrowed: Vec<_> = owned
            .iter()
            .map(|sample| FragSampleRef {
                data: &sample.data,
                duration: sample.duration,
                is_sync: sample.is_sync,
            })
            .collect();
        let mut source_bytes = Vec::new();
        let source_samples: Vec<_> = owned
            .iter()
            .map(|sample| {
                let offset = source_bytes.len() as u64;
                source_bytes.extend_from_slice(&sample.data);
                SourceSample {
                    offset,
                    size: sample.data.len() as u32,
                    duration: sample.duration,
                    is_sync: sample.is_sync,
                }
            })
            .collect();

        let mut owned_writer = HybridMp4Writer::new(Cursor::new(Vec::new()), video_cfg()).unwrap();
        owned_writer.write_fragment_multi(&[&owned]).unwrap();
        let expected = owned_writer.into_inner().into_inner();

        let mut borrowed_writer =
            HybridMp4Writer::new(Cursor::new(Vec::new()), video_cfg()).unwrap();
        borrowed_writer
            .write_fragment_multi_borrowed(&[&borrowed])
            .unwrap();
        assert_eq!(borrowed_writer.into_inner().into_inner(), expected);

        let mut source_writer = HybridMp4Writer::new(Cursor::new(Vec::new()), video_cfg()).unwrap();
        source_writer
            .write_fragment_multi_from_source(
                &mut Cursor::new(source_bytes.clone()),
                &[&source_samples],
            )
            .unwrap();
        assert_eq!(source_writer.into_inner().into_inner(), expected);

        let mut sources_writer =
            HybridMp4Writer::new(Cursor::new(Vec::new()), video_cfg()).unwrap();
        let mut source = Cursor::new(source_bytes);
        let mut sources: [&mut dyn ReadSeek; 1] = [&mut source];
        sources_writer
            .write_fragment_multi_from_sources(&mut sources, &[&source_samples])
            .unwrap();
        assert_eq!(sources_writer.into_inner().into_inner(), expected);
    }

    #[test]
    fn finalize_produces_ftyp_mdat_moov_layout() {
        let mut w = HybridMp4Writer::new(Cursor::new(Vec::new()), video_cfg()).unwrap();
        w.write_fragment(&gop(0)).unwrap();
        let buf = w.finalize().unwrap().into_inner();
        let boxes = walk(&buf);
        let fourccs: Vec<&[u8; 4]> = boxes.iter().map(|b| &b.fourcc).collect();
        assert_eq!(fourccs, vec![b"ftyp", b"mdat", b"moov"]);
    }

    #[test]
    fn finalize_multi_track_moov_has_one_trak_per_track() {
        let mut w = HybridMp4Writer::new_multi(
            Cursor::new(Vec::new()),
            vec![
                TrackConfig::Video(video_cfg()),
                TrackConfig::Audio(audio_cfg()),
            ],
        )
        .unwrap();
        let video = gop(0);
        let audio = all_sync_gop();
        w.write_fragment_multi(&[&video, &audio]).unwrap();
        let buf = w.finalize().unwrap().into_inner();

        let boxes = walk(&buf);
        let moov = find(&boxes, b"moov").unwrap();
        let kids = children(&buf, moov);
        let traks: Vec<_> = kids.iter().filter(|b| &b.fourcc == b"trak").collect();
        assert_eq!(traks.len(), 2, "one trak per track");
    }

    #[test]
    fn copy_exact_copies_all_bytes() {
        let src = b"hello world";
        let mut dst = Vec::new();
        let mut buf = [0u8; 4]; // small buffer forces multiple iterations
        copy_exact(&mut &src[..], &mut dst, src.len() as u64, &mut buf).unwrap();
        assert_eq!(dst, src);
    }

    #[test]
    fn sequence_number_increments_per_fragment() {
        let mut w = HybridMp4Writer::new(Cursor::new(Vec::new()), video_cfg()).unwrap();
        assert_eq!(w.next_sequence, 1);
        w.write_fragment(&gop(0)).unwrap();
        assert_eq!(w.next_sequence, 2);
        w.write_fragment(&gop(3)).unwrap();
        assert_eq!(w.next_sequence, 3);
    }

    #[test]
    fn next_decode_time_advances_by_sample_durations() {
        let mut w = HybridMp4Writer::new(Cursor::new(Vec::new()), video_cfg()).unwrap();
        w.write_fragment(&gop(0)).unwrap();
        // 3 samples × 3000 duration each
        assert_eq!(w.tracks[0].next_decode_time, 9000);
        w.write_fragment(&gop(3)).unwrap();
        assert_eq!(w.tracks[0].next_decode_time, 18000);
    }
}
