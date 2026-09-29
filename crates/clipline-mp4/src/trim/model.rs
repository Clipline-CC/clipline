use crate::{FragSample, SourceSample, TrackConfig};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaVideoCodec {
    H264,
    Hevc,
    Av1,
}

pub(crate) struct ParsedMovie {
    pub(crate) tracks: Vec<ParsedTrack>,
}

pub(crate) struct TrimSelection {
    pub(crate) video_idx: usize,
    pub(crate) start_idx: usize,
    pub(crate) end_idx: usize,
    pub(crate) aligned_start_s: f64,
    pub(crate) aligned_end_s: f64,
    pub(crate) aligned_start_ticks: u64,
    pub(crate) aligned_end_ticks: u64,
    pub(crate) video_timescale: u32,
}

pub(crate) struct ParsedTrack {
    pub(crate) cfg: TrackConfig,
    pub(crate) timescale: u32,
    pub(crate) samples: Vec<SampleRecord>,
}

#[derive(Clone)]
pub(crate) struct SampleRecord {
    pub(crate) offset: usize,
    pub(crate) size: u32,
    pub(crate) duration: u32,
    pub(crate) is_sync: bool,
    pub(crate) start_ticks: u64,
}

impl ParsedTrack {
    pub(crate) fn track_end_s(&self) -> f64 {
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

    pub(crate) fn to_frag_sample(&self, input: &[u8]) -> Result<FragSample, TrimError> {
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

    pub(crate) fn to_source_sample(&self) -> SourceSample {
        SourceSample {
            offset: self.offset as u64,
            size: self.size,
            duration: self.duration,
            is_sync: self.is_sync,
        }
    }
}

impl TrimSelection {
    pub(crate) fn info(&self, requested_start_s: f64, requested_end_s: f64) -> TrimInfo {
        TrimInfo {
            requested_start_s,
            requested_end_s,
            aligned_start_s: self.aligned_start_s,
            aligned_end_s: self.aligned_end_s,
            duration_s: self.aligned_end_s - self.aligned_start_s,
        }
    }

    pub(crate) fn contains_start(&self, start_ticks: u64, timescale: u32) -> bool {
        let start = u128::from(start_ticks) * u128::from(self.video_timescale);
        let lower = u128::from(self.aligned_start_ticks) * u128::from(timescale);
        let upper = u128::from(self.aligned_end_ticks) * u128::from(timescale);
        start >= lower && start < upper
    }

    pub(crate) fn rebase_start(&self, start_ticks: u64, timescale: u32) -> Result<u64, TrimError> {
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

pub(crate) fn select_trim_range(
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

pub(crate) fn seconds_to_ticks_floor(seconds: f64, timescale: u32) -> Result<u64, TrimError> {
    let ticks = seconds * f64::from(timescale);
    if !ticks.is_finite() || ticks < 0.0 || ticks > u64::MAX as f64 {
        return Err(TrimError::InvalidRange(
            "trim boundary is outside the supported timeline".into(),
        ));
    }
    Ok(ticks.floor() as u64)
}

pub(crate) fn seconds_to_ticks_ceil(seconds: f64, timescale: u32) -> Result<u64, TrimError> {
    let ticks = seconds * f64::from(timescale);
    if !ticks.is_finite() || ticks < 0.0 || ticks > u64::MAX as f64 {
        return Err(TrimError::InvalidRange(
            "trim boundary is outside the supported timeline".into(),
        ));
    }
    Ok(ticks.ceil() as u64)
}

pub(crate) fn rescale_ticks(
    value: u64,
    source_timescale: u32,
    target_timescale: u32,
) -> Result<u64, TrimError> {
    let scaled = u128::from(value) * u128::from(target_timescale) / u128::from(source_timescale);
    u64::try_from(scaled).map_err(|_| TrimError::Corrupt("timestamp rescale overflow".into()))
}

pub(crate) fn validate_range(start_s: f64, end_s: f64) -> Result<(), TrimError> {
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

#[cfg(test)]
pub(crate) mod fixtures {
    use super::super::parse::parse_movie;
    use crate::{
        AudioTrackConfig, FragSample, HybridMp4Writer, TrackConfig, VideoCodecParams,
        VideoTrackConfig,
    };
    use shiguredo_opus::{Decoder, DecoderConfig, Encoder, EncoderConfig};
    use std::io::Cursor;

        pub(crate) fn video_track() -> TrackConfig {
            TrackConfig::Video(VideoTrackConfig::h264(
                128,
                72,
                90_000,
                vec![0x67, 0x64, 0x00, 0x0A, 0xAC],
                vec![0x68, 0xEE, 0x38, 0x80],
            ))
        }

        pub(crate) fn audio_track() -> TrackConfig {
            TrackConfig::Audio(AudioTrackConfig {
                channels: 2,
                sample_rate: 48_000,
                pre_skip: 312,
            })
        }

        pub(crate) fn tracks() -> Vec<TrackConfig> {
            vec![video_track(), audio_track()]
        }

        pub(crate) fn video_gop(start: u32) -> Vec<FragSample> {
            (0..10)
                .map(|i| FragSample {
                    data: format!("V{:05}", start + i).into_bytes(),
                    duration: 9_000,
                    is_sync: i == 0,
                })
                .collect()
        }

        pub(crate) fn audio_packets(start: u32) -> Vec<FragSample> {
            audio_packets_with("A", start)
        }

        pub(crate) fn audio_packets_with(prefix: &str, start: u32) -> Vec<FragSample> {
            (0..50)
                .map(|i| FragSample {
                    data: format!("{prefix}{:05}", start + i).into_bytes(),
                    duration: 960,
                    is_sync: true,
                })
                .collect()
        }

        pub(crate) fn opus_audio_packets(amplitude: f32) -> Vec<FragSample> {
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

        pub(crate) fn clipline_two_real_opus_audio_fixture() -> Vec<u8> {
            let mut w =
                HybridMp4Writer::new_multi(Cursor::new(Vec::new()), tracks_two_audio()).unwrap();
            let v = video_gop(0);
            let output = opus_audio_packets(0.20);
            let mic = opus_audio_packets(0.25);
            w.write_fragment_multi(&[&v, &output, &mic]).unwrap();
            w.finalize().unwrap().into_inner()
        }

        pub(crate) fn clipline_staggered_opus_audio_fixture() -> Vec<u8> {
            let mut w =
                HybridMp4Writer::new_multi(Cursor::new(Vec::new()), tracks_two_audio()).unwrap();
            let v = video_gop(0);
            let output = opus_audio_packets(0.20);
            let mic = opus_audio_packets(0.25);
            w.set_track_decode_time(2, 480).unwrap();
            w.write_fragment_multi(&[&v, &output, &mic]).unwrap();
            w.finalize().unwrap().into_inner()
        }

        pub(crate) fn clipline_gapped_opus_audio_fixture() -> Vec<u8> {
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

        pub(crate) fn decoded_audible_audio_rms(input: &[u8]) -> f64 {
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

        pub(crate) fn first_audio_config(input: &[u8]) -> AudioTrackConfig {
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

        pub(crate) fn clipline_fixture() -> Vec<u8> {
            let mut w = HybridMp4Writer::new_multi(Cursor::new(Vec::new()), tracks()).unwrap();
            for second in 0..3 {
                let v = video_gop(second * 10);
                let a = audio_packets(second * 50);
                w.write_fragment_multi(&[&v, &a]).unwrap();
            }
            w.finalize().unwrap().into_inner()
        }

        pub(crate) fn clipline_gap_fixture() -> Vec<u8> {
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

        pub(crate) fn tracks_two_audio() -> Vec<TrackConfig> {
            vec![video_track(), audio_track(), audio_track()]
        }

        pub(crate) fn audio_only_tracks() -> Vec<TrackConfig> {
            vec![audio_track()]
        }

        pub(crate) fn clipline_two_audio_fixture() -> Vec<u8> {
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

        pub(crate) fn clipline_audio_only_fixture() -> Vec<u8> {
            let mut w =
                HybridMp4Writer::new_multi(Cursor::new(Vec::new()), audio_only_tracks()).unwrap();
            for second in 0..2 {
                let audio = audio_packets(second * 50);
                w.write_fragment_multi(&[&audio]).unwrap();
            }
            w.finalize().unwrap().into_inner()
        }

        // Real x265 / SVT-AV1 parameter sets (128x72) so the round-trip parses
        // genuine hvcC / av1C records, not just placeholder bytes.

        pub(crate) const HEVC_VPS: &[u8] = &[0x40, 0x01, 0x0C, 0x01, 0xFF, 0xFF, 0x01];

        pub(crate) const HEVC_SPS: &[u8] = &[
            0x42, 0x01, 0x01, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0x90, 0x00, 0x00, 0x03, 0x00, 0x00,
            0x03, 0x00, 0x1E, 0xA0, 0x10, 0x20, 0x49, 0x65, 0x95, 0x9A, 0x49, 0x32, 0xBC, 0x05, 0xA0,
            0x20, 0x00, 0x00, 0x03, 0x00, 0x20, 0x00, 0x00, 0x03, 0x03, 0xC1,
        ];

        pub(crate) const HEVC_PPS: &[u8] = &[0x44, 0x01, 0xC1, 0x72, 0xB4, 0x22, 0x40];

        pub(crate) const AV1_SEQ_OBU: &[u8] = &[
            0x0A, 0x0A, 0x00, 0x00, 0x00, 0x03, 0x37, 0xF8, 0xE3, 0x57, 0xCC, 0x02,
        ];

        pub(crate) fn single_video_fixture(codec: VideoCodecParams) -> Vec<u8> {
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
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::super::parse::parse_movie;
    use super::super::{remux_with_selected_audio_tracks, trim_keyframe_aligned};
    use super::*;
    use crate::{VideoCodecParams, VideoTrackConfig};

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
}
