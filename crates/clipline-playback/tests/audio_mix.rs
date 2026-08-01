use std::path::PathBuf;

use clipline_mp4::{IndexedMovie, PlaybackTime, PlaybackTrackConfig, TrackSampleRange};
use clipline_playback::{
    mix_stereo_frames_into, AudioError, AudioResetPoint, AudioTrackSpec, IndexedAudioPacketReader,
    OpusDecoderBank, OpusTrackDecoder, StereoRingBuffer, TimelineAudioMixer, WorkGeneration,
    MAX_AUDIO_QUEUE_FRAMES, MAX_OPUS_PACKET_BYTES,
};
use shiguredo_opus::{Encoder, EncoderConfig};

const OPEN_GENERATION: WorkGeneration = WorkGeneration::new(1, 0);
const SEEK_GENERATION: WorkGeneration = WorkGeneration::new(1, 1);

fn encode(channels: u8, pcm: &[f32]) -> Vec<u8> {
    Encoder::new(EncoderConfig::new(48_000, channels))
        .unwrap()
        .encode_f32(pcm)
        .unwrap()
}

#[test]
fn opus_decoder_converts_mono_to_stereo_and_preserves_stereo_channels() {
    let mono_packet = encode(1, &vec![0.25; 960]);
    let mut mono = OpusTrackDecoder::new(1, 48_000, 0, OPEN_GENERATION).unwrap();
    let decoded = mono.decode(&mono_packet, OPEN_GENERATION).unwrap();
    assert_eq!(decoded.len(), 960 * 2);
    assert!(decoded
        .chunks_exact(2)
        .all(|pair| pair[0].to_bits() == pair[1].to_bits()));

    let stereo_pcm: Vec<_> = (0..960).flat_map(|_| [0.25, -0.25]).collect();
    let stereo_packet = encode(2, &stereo_pcm);
    let mut stereo = OpusTrackDecoder::new(2, 48_000, 0, OPEN_GENERATION).unwrap();
    let decoded = stereo.decode(&stereo_packet, OPEN_GENERATION).unwrap();
    assert_eq!(decoded.len(), 960 * 2);
    let (left, right) = decoded
        .chunks_exact(2)
        .fold((0.0, 0.0), |sum, pair| (sum.0 + pair[0], sum.1 + pair[1]));
    assert!(left > 0.0);
    assert!(right < 0.0);
}

#[test]
fn pre_skip_applies_only_at_file_start_and_reset_is_generation_fenced() {
    let packet = encode(2, &vec![0.2; 960 * 2]);
    let mut decoder = OpusTrackDecoder::new(2, 48_000, 312, OPEN_GENERATION).unwrap();
    assert_eq!(
        decoder.decode(&packet, OPEN_GENERATION).unwrap().len(),
        (960 - 312) * 2
    );
    assert_eq!(
        decoder.decode(&packet, OPEN_GENERATION).unwrap().len(),
        960 * 2
    );

    decoder
        .reset(SEEK_GENERATION, AudioResetPoint::MidStream)
        .unwrap();
    assert!(matches!(
        decoder.decode(&packet, OPEN_GENERATION),
        Err(AudioError::StaleGeneration { .. })
    ));
    assert_eq!(
        decoder.decode(&packet, SEEK_GENERATION).unwrap().len(),
        960 * 2
    );

    let reopen = WorkGeneration::new(2, 0);
    decoder.reset(reopen, AudioResetPoint::FileStart).unwrap();
    assert_eq!(
        decoder.decode(&packet, reopen).unwrap().len(),
        (960 - 312) * 2
    );
    let stats = decoder.stats();
    assert_eq!(stats.decoded_packets, 4);
    assert_eq!(stats.pre_skip_frames, 624);
    assert_eq!(stats.resets, 2);
}

#[test]
fn corrupt_and_hostile_packets_fail_without_publishable_pcm() {
    let mut decoder = OpusTrackDecoder::new(2, 48_000, 0, OPEN_GENERATION).unwrap();
    assert!(matches!(
        decoder.decode(&[], OPEN_GENERATION),
        Err(AudioError::Decode { .. })
    ));
    assert_eq!(decoder.output_len(), 0);
    assert!(matches!(
        decoder.decode(&vec![0; MAX_OPUS_PACKET_BYTES + 1], OPEN_GENERATION),
        Err(AudioError::PacketTooLarge { .. })
    ));
    assert_eq!(decoder.output_len(), 0);
    assert_eq!(decoder.stats().corrupt_packets, 1);
    assert_eq!(decoder.stats().resets, 1);

    let exact_cap_error = decoder
        .decode(&vec![0; MAX_OPUS_PACKET_BYTES], OPEN_GENERATION)
        .unwrap_err();
    assert!(!matches!(
        exact_cap_error,
        AudioError::PacketTooLarge { .. }
    ));
}

#[test]
fn indexed_corruption_consumes_pre_skip_and_recovers_on_the_next_packet() {
    let valid_packet = encode(2, &vec![0.2; 960 * 2]);
    let mut decoder = OpusTrackDecoder::new(2, 48_000, 312, OPEN_GENERATION).unwrap();

    assert!(matches!(
        decoder.decode_indexed(&[], 960, OPEN_GENERATION),
        Err(AudioError::Decode { .. })
    ));
    assert_eq!(decoder.output_len(), 0);
    assert_eq!(
        decoder.stats(),
        clipline_playback::OpusDecodeStats {
            pre_skip_frames: 312,
            corrupt_packets: 1,
            corrupt_frames: 648,
            resets: 1,
            ..Default::default()
        }
    );

    let recovered = decoder
        .decode_indexed(&valid_packet, 960, OPEN_GENERATION)
        .unwrap();
    assert_eq!(recovered.len(), 960 * 2);
    assert_eq!(decoder.stats().decoded_frames, 960);
    assert_eq!(decoder.stats().pre_skip_frames, 312);
}

#[test]
fn mix_averages_active_tracks_treats_absence_as_silence_and_clamps() {
    let first = [0.7, -0.7, 2.0, -2.0];
    let second = [0.5, -0.5, 2.0, -2.0];
    let mut output = [0.0; 4];

    let outcome = mix_stereo_frames_into(&[Some(&first), Some(&second)], 2, &mut output).unwrap();
    assert_eq!(outcome.active_tracks, 2);
    assert_eq!(output, [0.6, -0.6, 1.0, -1.0]);

    mix_stereo_frames_into(&[Some(&first), None], 2, &mut output).unwrap();
    assert_eq!(output, [0.7, -0.7, 1.0, -1.0]);
    mix_stereo_frames_into(&[None, None], 2, &mut output).unwrap();
    assert_eq!(output, [0.0; 4]);
    assert!(matches!(
        mix_stereo_frames_into(&[Some(&first[..2])], 2, &mut output),
        Err(AudioError::FrameLengthMismatch { .. })
    ));
    let before = output;
    assert!(matches!(
        mix_stereo_frames_into(
            &[Some(&[f32::NAN, 0.0, 0.0, f32::INFINITY])],
            2,
            &mut output
        ),
        Err(AudioError::NonFiniteSample { .. })
    ));
    assert_eq!(output, before);
}

#[test]
fn fixed_ring_wraps_is_caller_drained_and_never_exceeds_500ms() {
    assert!(matches!(
        StereoRingBuffer::new(MAX_AUDIO_QUEUE_FRAMES + 1),
        Err(AudioError::InvalidQueueCapacity { .. })
    ));
    let mut ring = StereoRingBuffer::new(4).unwrap();
    ring.push_interleaved(&[1.0, 10.0, 2.0, 20.0, 3.0, 30.0])
        .unwrap();
    let mut first = [0.0; 4];
    assert_eq!(ring.drain_into(&mut first).unwrap(), 2);
    assert_eq!(first, [1.0, 10.0, 2.0, 20.0]);
    ring.push_interleaved(&[4.0, 40.0, 5.0, 50.0, 6.0, 60.0])
        .unwrap();
    assert!(matches!(
        ring.push_interleaved(&[7.0, 70.0]),
        Err(AudioError::QueueFull { .. })
    ));
    let mut rest = [0.0; 8];
    assert_eq!(ring.drain_into(&mut rest).unwrap(), 4);
    assert_eq!(rest, [3.0, 30.0, 4.0, 40.0, 5.0, 50.0, 6.0, 60.0]);
    let telemetry = ring.telemetry();
    assert_eq!(telemetry.capacity_frames, 4);
    assert_eq!(telemetry.high_water_frames, 4);
    assert_eq!(telemetry.allocation_count, 1);
}

#[test]
fn timeline_mixer_inserts_leading_internal_absent_and_eof_silence() {
    let mut mixer = TimelineAudioMixer::new(32, 0).unwrap();
    let active = [0.5, -0.5, 0.25, -0.25];

    mixer.mix_at(3, 2, &[Some(&active), None]).unwrap();
    mixer.mix_at(7, 2, &[None, None]).unwrap();
    mixer.finish_at(11).unwrap();

    let mut output = [99.0; 22];
    assert_eq!(mixer.drain_into(&mut output).unwrap(), 11);
    assert_eq!(&output[..6], &[0.0; 6]);
    assert_eq!(&output[6..10], &active);
    assert_eq!(&output[10..], &[0.0; 12]);
    let stats = mixer.stats();
    assert_eq!(stats.mixed_frames, 2);
    assert_eq!(stats.silent_frames, 9);
    assert_eq!(stats.timeline_frames, 11);
    assert_eq!(stats.queue_high_water_frames, 11);
}

#[test]
fn timeline_reset_drops_stale_premixed_audio_before_track_switch() {
    let mut mixer = TimelineAudioMixer::new(8, 10).unwrap();
    mixer.mix_at(10, 2, &[Some(&[0.5, 0.5, 0.5, 0.5])]).unwrap();
    assert_eq!(mixer.queued_frames(), 2);
    assert_eq!(mixer.reset_at(20), 2);
    assert_eq!(mixer.queued_frames(), 0);
    mixer.mix_at(20, 1, &[None]).unwrap();
    let mut output = [1.0; 2];
    assert_eq!(mixer.drain_into(&mut output).unwrap(), 1);
    assert_eq!(output, [0.0; 2]);
    assert_eq!(mixer.stats().dropped_frames, 2);
}

#[test]
fn track_switching_resets_every_participant_in_the_new_mix_generation() {
    let packet = encode(2, &vec![0.2; 960 * 2]);
    let output = AudioTrackSpec::new(1, 2, 48_000, 312).unwrap();
    let microphone = AudioTrackSpec::new(2, 2, 48_000, 312).unwrap();
    let commentary = AudioTrackSpec::new(3, 2, 48_000, 0).unwrap();
    let mut bank = OpusDecoderBank::new();

    let initial = bank
        .select_tracks(
            &[output, microphone],
            OPEN_GENERATION,
            AudioResetPoint::FileStart,
        )
        .unwrap();
    assert_eq!(initial.added, 2);
    assert_eq!(initial.kept, 0);
    bank.decode(1, &packet, OPEN_GENERATION).unwrap();
    bank.decode(2, &packet, OPEN_GENERATION).unwrap();

    let selection_generation = WorkGeneration::new(1, 1);
    let changed = bank
        .select_tracks(&[output], selection_generation, AudioResetPoint::MidStream)
        .unwrap();
    assert_eq!((changed.kept, changed.added, changed.removed), (1, 0, 1));
    assert_eq!(bank.decoder_stats(1).unwrap().resets, 1);
    assert!(matches!(
        bank.decode(2, &packet, selection_generation),
        Err(AudioError::MissingDecoder { track_index: 2 })
    ));
    assert_eq!(
        bank.decode(1, &packet, selection_generation).unwrap().len(),
        960 * 2
    );

    let added = bank
        .select_tracks(
            &[output, commentary],
            selection_generation,
            AudioResetPoint::MidStream,
        )
        .unwrap();
    assert_eq!((added.kept, added.added, added.removed), (1, 1, 0));
    assert_eq!(bank.decoder_stats(1).unwrap().resets, 2);
    assert_eq!(bank.decoder_stats(3).unwrap().resets, 0);

    let seek_generation = WorkGeneration::new(1, 2);
    bank.reset_for_seek(seek_generation, AudioResetPoint::MidStream)
        .unwrap();
    assert_eq!(bank.decoder_stats(1).unwrap().resets, 3);
    assert_eq!(bank.decoder_stats(3).unwrap().resets, 1);
    assert_eq!(bank.selected_track_indices(), vec![1, 3]);
    assert_eq!(bank.stats().decoded_packets, 3);
}

#[test]
fn track_selection_rejects_duplicates_without_mutating_decoder_bank() {
    let output = AudioTrackSpec::new(1, 2, 48_000, 312).unwrap();
    let mut bank = OpusDecoderBank::new();
    bank.select_tracks(&[output], OPEN_GENERATION, AudioResetPoint::FileStart)
        .unwrap();

    assert!(matches!(
        bank.select_tracks(
            &[output, output],
            SEEK_GENERATION,
            AudioResetPoint::MidStream
        ),
        Err(AudioError::DuplicateTrack { track_index: 1 })
    ));
    assert_eq!(bank.selected_track_indices(), vec![1]);
    assert_eq!(bank.decoder_stats(1).unwrap().resets, 0);
}

#[test]
fn audio_formats_and_selection_count_are_bounded_before_decoder_creation() {
    assert!(matches!(
        AudioTrackSpec::new(1, 0, 48_000, 0),
        Err(AudioError::InvalidChannels { channels: 0 })
    ));
    assert!(matches!(
        AudioTrackSpec::new(1, 2, 44_100, 0),
        Err(AudioError::UnsupportedSampleRate {
            sample_rate: 44_100
        })
    ));

    let specs: Vec<_> = (0..9)
        .map(|track_index| AudioTrackSpec::new(track_index, 2, 48_000, 0).unwrap())
        .collect();
    let mut bank = OpusDecoderBank::new();
    assert!(matches!(
        bank.select_tracks(&specs, OPEN_GENERATION, AudioResetPoint::FileStart),
        Err(AudioError::TooManyTracks {
            selected: 9,
            limit: 8
        })
    ));
    assert!(bank.selected_track_indices().is_empty());
}

#[test]
fn production_fixture_decodes_trims_and_mixes_to_exact_five_second_timeline() {
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/playback/hybrid-writer-h264-two-opus-5s.mp4");
    let movie = IndexedMovie::open(fixture).unwrap();
    let audio_track_indices: Vec<_> = movie
        .index()
        .tracks
        .iter()
        .enumerate()
        .filter_map(|(index, track)| {
            matches!(track.config, PlaybackTrackConfig::Opus { .. }).then_some(index)
        })
        .collect();
    assert_eq!(audio_track_indices.len(), 2);

    let mut specs = Vec::new();
    let mut ranges = Vec::new();
    for &track_index in &audio_track_indices {
        let track = &movie.index().tracks[track_index];
        let PlaybackTrackConfig::Opus {
            channels,
            sample_rate,
            pre_skip,
        } = track.config
        else {
            unreachable!();
        };
        assert_eq!(track.samples.len(), 251);
        assert_eq!(pre_skip, 312);
        specs.push(AudioTrackSpec::new(track_index, channels, sample_rate, pre_skip).unwrap());
        ranges.push(TrackSampleRange {
            track_index,
            samples: 0..track.samples.len(),
        });
    }

    let timeline_end = PlaybackTime::new(240_000, 48_000).unwrap();
    let mut reader =
        IndexedAudioPacketReader::new(movie, ranges, timeline_end, OPEN_GENERATION).unwrap();
    let mut bank = OpusDecoderBank::new();
    bank.select_tracks(&specs, OPEN_GENERATION, AudioResetPoint::FileStart)
        .unwrap();
    let mut mixer = TimelineAudioMixer::new(MAX_AUDIO_QUEUE_FRAMES, 0).unwrap();
    let mut mixed = vec![0.0; 5_760 * 2];
    let mut drained = vec![0.0; 5_760 * 2];
    let mut drained_frames = 0_usize;
    let mut audible_energy = 0.0_f64;

    for sample_index in 0..251 {
        bank.clear_pending_frames();
        let mut audible_start = None;
        for &track_index in &audio_track_indices {
            let packet = reader
                .read_packet(track_index, sample_index, OPEN_GENERATION)
                .unwrap();
            audible_start.get_or_insert(packet.audible_start_tick);
            bank.decode_indexed(
                track_index,
                packet.bytes,
                packet.indexed_duration_frames,
                OPEN_GENERATION,
            )
            .unwrap();
        }
        let frames = bank.pending_frames(audio_track_indices[0]).unwrap();
        assert_eq!(bank.pending_frames(audio_track_indices[1]).unwrap(), frames);
        bank.mix_pending_into(&audio_track_indices, frames, &mut mixed[..frames * 2])
            .unwrap();
        mixer
            .mix_at(
                audible_start.unwrap(),
                frames,
                &[Some(&mixed[..frames * 2])],
            )
            .unwrap();
        let frames = mixer.drain_into(&mut drained).unwrap();
        drained_frames += frames;
        audible_energy += drained[..frames * 2]
            .iter()
            .map(|sample| f64::from(sample.abs()))
            .sum::<f64>();
    }
    mixer.finish_at(240_000).unwrap();
    drained_frames += mixer.drain_into(&mut drained).unwrap();

    assert_eq!(drained_frames, 240_000);
    assert!(audible_energy > 1.0);
    assert_eq!(bank.stats().decoded_packets, 502);
    assert_eq!(bank.stats().decoded_frames, 480_000);
    assert_eq!(bank.stats().pre_skip_frames, 624);
    assert_eq!(bank.stats().corrupt_packets, 0);
    let mix_stats = mixer.stats();
    assert_eq!(mix_stats.mixed_frames, 240_000);
    assert_eq!(mix_stats.silent_frames, 0);
    assert_eq!(mix_stats.timeline_frames, 240_000);
    assert!(mix_stats.queue_high_water_frames <= MAX_AUDIO_QUEUE_FRAMES);
    let packet_stats = reader.telemetry();
    assert_eq!(packet_stats.packets_read, 502);
    assert_eq!(packet_stats.packet_high_water, 160);
    assert_eq!(packet_stats.packet_reserve_count, 1);
    assert!(packet_stats.packet_high_water <= packet_stats.logical_packet_limit);
}

#[test]
fn indexed_packet_reader_enforces_ranges_timeline_and_generation_before_io() {
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/playback/hybrid-writer-h264-two-opus-5s.mp4");
    let movie = IndexedMovie::open(&fixture).unwrap();
    let track_index = movie
        .index()
        .tracks
        .iter()
        .position(|track| matches!(track.config, PlaybackTrackConfig::Opus { .. }))
        .unwrap();
    let mut reader = IndexedAudioPacketReader::new(
        movie,
        vec![TrackSampleRange {
            track_index,
            samples: 10..12,
        }],
        PlaybackTime::new(240_000, 48_000).unwrap(),
        OPEN_GENERATION,
    )
    .unwrap();

    assert!(matches!(
        reader.read_packet(track_index, 9, OPEN_GENERATION),
        Err(AudioError::PacketOutsideRange { .. })
    ));
    assert_eq!(reader.telemetry().packets_read, 0);
    assert_eq!(
        reader
            .read_packet(track_index, 10, OPEN_GENERATION)
            .unwrap()
            .bytes
            .len(),
        160
    );
    reader.reset_generation(SEEK_GENERATION);
    assert!(matches!(
        reader.read_packet(track_index, 11, OPEN_GENERATION),
        Err(AudioError::StaleGeneration { .. })
    ));
    assert_eq!(reader.telemetry().packets_read, 1);
    assert_eq!(
        reader
            .read_packet(track_index, 11, SEEK_GENERATION)
            .unwrap()
            .bytes
            .len(),
        160
    );
    assert!(matches!(
        reader.read_packet(track_index, 12, SEEK_GENERATION),
        Err(AudioError::PacketOutsideRange { .. })
    ));

    let movie = IndexedMovie::open(fixture).unwrap();
    let mut ended = IndexedAudioPacketReader::new(
        movie,
        vec![TrackSampleRange {
            track_index,
            samples: 0..1,
        }],
        PlaybackTime::new(0, 48_000).unwrap(),
        OPEN_GENERATION,
    )
    .unwrap();
    assert!(matches!(
        ended.read_packet(track_index, 0, OPEN_GENERATION),
        Err(AudioError::PacketBeyondTimeline { .. })
    ));
    assert_eq!(ended.telemetry().packets_read, 0);
}

#[test]
fn indexed_audio_ranges_are_reselected_transactionally_for_each_seek_plan() {
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/playback/hybrid-writer-h264-two-opus-5s.mp4");
    let movie = IndexedMovie::open(fixture).unwrap();
    let track_index = movie
        .index()
        .tracks
        .iter()
        .position(|track| matches!(track.config, PlaybackTrackConfig::Opus { .. }))
        .unwrap();
    let mut reader = IndexedAudioPacketReader::new(
        movie,
        vec![TrackSampleRange {
            track_index,
            samples: 10..12,
        }],
        PlaybackTime::new(240_000, 48_000).unwrap(),
        OPEN_GENERATION,
    )
    .unwrap();
    let warmed = reader.telemetry();

    reader
        .reselect_ranges(
            vec![TrackSampleRange {
                track_index,
                samples: 20..22,
            }],
            PlaybackTime::new(240_000, 48_000).unwrap(),
            SEEK_GENERATION,
        )
        .unwrap();
    assert!(matches!(
        reader.read_packet(track_index, 10, SEEK_GENERATION),
        Err(AudioError::PacketOutsideRange { .. })
    ));
    assert_eq!(
        reader
            .read_packet(track_index, 20, SEEK_GENERATION)
            .unwrap()
            .bytes
            .len(),
        160
    );
    assert_eq!(
        reader.telemetry().packet_reserve_count,
        warmed.packet_reserve_count
    );

    let rejected_generation = WorkGeneration::new(1, 2);
    assert!(matches!(
        reader.reselect_ranges(
            vec![
                TrackSampleRange {
                    track_index,
                    samples: 30..31,
                },
                TrackSampleRange {
                    track_index,
                    samples: 31..32,
                },
            ],
            PlaybackTime::new(240_000, 48_000).unwrap(),
            rejected_generation,
        ),
        Err(AudioError::DuplicatePacketRange { .. })
    ));
    assert_eq!(
        reader
            .read_packet(track_index, 21, SEEK_GENERATION)
            .unwrap()
            .bytes
            .len(),
        160,
        "a rejected selection must not mutate the live range or generation"
    );
}
