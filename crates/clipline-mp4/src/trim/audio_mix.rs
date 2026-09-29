use std::io::{Cursor, Read, Seek, SeekFrom, Write};

use shiguredo_opus::{Decoder, DecoderConfig, Encoder, EncoderConfig};
use crate::{AudioTrackConfig, FragSample, SourceSample, TrackConfig};
use super::model::{ParsedTrack, SampleRecord, TrimError};

const MAX_OPUS_PACKET_BYTES: u32 = 1024 * 1024;

const MAX_OPUS_FRAME_TICKS: u32 = 5_760;

const OPUS_MIX_FRAME_TICKS: u32 = 960;

pub(crate) struct MixedAudioTrack {
    pub(crate) cfg: AudioTrackConfig,
    pub(crate) samples: Vec<FragSample>,
    pub(crate) start_ticks: Vec<u64>,
}

pub(crate) struct MixedAudioSource {
    pub(crate) cfg: AudioTrackConfig,
    pub(crate) samples: Vec<SourceSample>,
    pub(crate) start_ticks: Vec<u64>,
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

pub(crate) fn mix_selected_opus_audio_tracks_to_spool<R: Read + Seek, W: Write + Seek>(
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

pub(crate) fn mix_selected_opus_audio_tracks(
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

#[cfg(test)]
mod tests {
    use super::super::model::fixtures::*;
    use super::super::parse::parse_movie;
    use super::super::remux_with_mixed_audio_track;
    use super::*;

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
