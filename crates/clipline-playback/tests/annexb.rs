use std::path::PathBuf;

use clipline_mp4::{IndexedMovie, PlaybackTrackConfig, SampleIndex, TrackIndex};
use clipline_playback::{
    plan_video_sample_buffers, AnnexBError, AnnexBLimits, H264AnnexBConverter, H264DecoderConfig,
    NativeVideoCapability, UnsupportedVideoCodec, VideoSampleTransport, WorkGeneration,
};

const OPEN_GENERATION: WorkGeneration = WorkGeneration::new(1, 0);
const SEEK_GENERATION: WorkGeneration = WorkGeneration::new(1, 1);

fn h264_config(nal_length_size: u8) -> H264DecoderConfig {
    H264DecoderConfig::new(
        1920,
        1080,
        nal_length_size,
        vec![vec![0x67, 0x64, 0x00, 0x28]],
        vec![vec![0x68, 0xee, 0x3c, 0x80]],
    )
    .unwrap()
}

fn converter(nal_length_size: u8) -> H264AnnexBConverter {
    H264AnnexBConverter::new(
        h264_config(nal_length_size),
        AnnexBLimits::default(),
        OPEN_GENERATION,
    )
    .unwrap()
}

fn length_prefixed(nal_length_size: usize, nals: &[&[u8]]) -> Vec<u8> {
    let mut sample = Vec::new();
    for nal in nals {
        let length = u32::try_from(nal.len()).unwrap().to_be_bytes();
        sample.extend_from_slice(&length[4 - nal_length_size..]);
        sample.extend_from_slice(nal);
    }
    sample
}

fn expected_annex_b(nals: &[&[u8]]) -> Vec<u8> {
    let mut output = Vec::new();
    for nal in nals {
        output.extend_from_slice(&[0, 0, 0, 1]);
        output.extend_from_slice(nal);
    }
    output
}

fn convert_expected_sample(sample: &[u8], nal_length_size: usize) -> Vec<u8> {
    let mut nals = Vec::new();
    let mut offset = 0;
    while offset < sample.len() {
        let mut length = 0_usize;
        for byte in &sample[offset..offset + nal_length_size] {
            length = (length << 8) | usize::from(*byte);
        }
        offset += nal_length_size;
        nals.push(&sample[offset..offset + length]);
        offset += length;
    }
    expected_annex_b(&nals)
}

#[test]
fn converts_one_two_and_four_byte_lengths_and_multiple_nals() {
    let nals: &[&[u8]] = &[&[0x65, 1, 2, 3], &[0x41, 4, 5]];
    let expected = expected_annex_b(nals);

    for length_size in [1_u8, 2, 4] {
        let mut converter = converter(length_size);
        let sample = length_prefixed(length_size as usize, nals);
        assert_eq!(
            converter
                .convert(&sample, false, OPEN_GENERATION)
                .unwrap()
                .bytes,
            expected
        );
    }

    for (length_size, payload_size) in [(1_u8, 255_usize), (2, 65_535)] {
        let payload = vec![0x41; payload_size];
        let sample = length_prefixed(length_size as usize, &[&payload]);
        let mut converter = converter(length_size);
        let converted = converter.convert(&sample, false, OPEN_GENERATION).unwrap();
        assert_eq!(converted.bytes.len(), payload_size + 4);
        assert_eq!(&converted.bytes[4..], payload);
    }
}

#[test]
fn rejects_empty_truncated_zero_overflow_and_trailing_data_atomically() {
    let mut converter = converter(4);
    assert!(matches!(
        converter.convert(&[], false, OPEN_GENERATION),
        Err(AnnexBError::EmptySample)
    ));
    assert!(matches!(
        converter.convert(&[0, 0], false, OPEN_GENERATION),
        Err(AnnexBError::TruncatedLength { .. })
    ));
    assert!(matches!(
        converter.convert(&[0, 0, 0, 0], false, OPEN_GENERATION),
        Err(AnnexBError::ZeroLengthNal { .. })
    ));
    assert!(matches!(
        converter.convert(&[0, 0, 0, 5, 1, 2], false, OPEN_GENERATION),
        Err(AnnexBError::NalLengthExceedsSample { .. })
    ));
    assert!(matches!(
        converter.convert(&[0xff, 0xff, 0xff, 0xff, 1], false, OPEN_GENERATION),
        Err(AnnexBError::NalLengthExceedsSample {
            declared: 4_294_967_295,
            ..
        })
    ));
    assert!(matches!(
        converter.convert(&[0, 0, 0, 1, 0x41, 0, 0], false, OPEN_GENERATION),
        Err(AnnexBError::TrailingBytes { count: 2 })
    ));
    assert_eq!(converter.output_len(), 0);
}

#[test]
fn exact_output_cap_succeeds_and_cap_overflow_preserves_pending_injection() {
    let config = H264DecoderConfig::new(64, 64, 1, vec![vec![0x67]], vec![vec![0x68]]).unwrap();
    let mut converter =
        H264AnnexBConverter::new(config, AnnexBLimits::new(64, 20).unwrap(), OPEN_GENERATION)
            .unwrap();
    let many_tiny_nals = length_prefixed(1, &[&[1], &[2], &[3], &[4]]);
    assert_eq!(
        converter
            .convert(&many_tiny_nals, false, OPEN_GENERATION)
            .unwrap()
            .bytes
            .len(),
        20
    );

    let sync = length_prefixed(1, &[&[0x65; 7]]);
    assert!(matches!(
        converter.convert(&sync, true, OPEN_GENERATION),
        Err(AnnexBError::OutputTooLarge {
            required: 21,
            limit: 20
        })
    ));
    assert!(converter.parameter_sets_pending());
    assert_eq!(converter.output_len(), 0);
}

#[test]
fn parameter_set_injection_commits_only_after_same_generation_decoder_acceptance() {
    let config = H264DecoderConfig::new(
        1280,
        720,
        2,
        vec![vec![0x67, 1], vec![0x67, 2]],
        vec![vec![0x68, 1], vec![0x68, 2]],
    )
    .unwrap();
    let mut converter =
        H264AnnexBConverter::new(config, AnnexBLimits::default(), OPEN_GENERATION).unwrap();
    let inter = length_prefixed(2, &[&[0x41, 9]]);
    let sync = length_prefixed(2, &[&[0x65, 7]]);
    let injected = expected_annex_b(&[&[0x67, 1], &[0x67, 2], &[0x68, 1], &[0x68, 2], &[0x65, 7]]);

    assert_eq!(
        converter
            .convert(&inter, false, OPEN_GENERATION)
            .unwrap()
            .bytes,
        expected_annex_b(&[&[0x41, 9]])
    );
    let first_submission = {
        let unit = converter.convert(&sync, true, OPEN_GENERATION).unwrap();
        assert_eq!(unit.bytes, injected);
        unit.parameter_set_submission.unwrap()
    };
    assert!(converter.parameter_sets_pending());

    let retry_submission = converter
        .convert(&sync, true, OPEN_GENERATION)
        .unwrap()
        .parameter_set_submission
        .unwrap();
    assert!(!converter.commit_parameter_sets(first_submission));
    assert!(converter.commit_parameter_sets(retry_submission));
    assert!(!converter.parameter_sets_pending());
    assert_eq!(
        converter
            .convert(&sync, true, OPEN_GENERATION)
            .unwrap()
            .bytes,
        expected_annex_b(&[&[0x65, 7]])
    );

    converter.reset_for_generation(SEEK_GENERATION);
    assert!(!converter.commit_parameter_sets(retry_submission));
    assert!(matches!(
        converter.convert(&sync, true, OPEN_GENERATION),
        Err(AnnexBError::StaleGeneration { .. })
    ));
    assert_eq!(
        converter
            .convert(&inter, false, SEEK_GENERATION)
            .unwrap()
            .bytes,
        expected_annex_b(&[&[0x41, 9]])
    );
    assert!(converter.parameter_sets_pending());
    let seek_submission = {
        let unit = converter.convert(&sync, true, SEEK_GENERATION).unwrap();
        assert_eq!(unit.bytes, injected);
        unit.parameter_set_submission.unwrap()
    };
    assert!(converter.commit_parameter_sets(seek_submission));
}

#[test]
fn forged_h264_configuration_fails_before_capability_advertisement() {
    for invalid_length_size in [0, 3, 5, u8::MAX] {
        assert!(matches!(
            H264DecoderConfig::new(
                1920,
                1080,
                invalid_length_size,
                vec![vec![0x67]],
                vec![vec![0x68]]
            ),
            Err(AnnexBError::InvalidNalLengthSize(_))
        ));
    }
    assert!(matches!(
        H264DecoderConfig::new(0, 1080, 4, vec![vec![0x67]], vec![vec![0x68]]),
        Err(AnnexBError::InvalidDimensions { .. })
    ));
    assert!(matches!(
        H264DecoderConfig::new(1920, 1080, 4, vec![], vec![vec![0x68]]),
        Err(AnnexBError::MissingParameterSets)
    ));
    assert!(matches!(
        H264DecoderConfig::new(1920, 1080, 4, vec![vec![]], vec![vec![0x68]]),
        Err(AnnexBError::EmptyParameterSet { .. })
    ));
    assert!(matches!(
        H264DecoderConfig::new(1920, 1080, 4, vec![vec![0x68]], vec![vec![0x68]]),
        Err(AnnexBError::UnexpectedParameterSetType { .. })
    ));

    let forged = PlaybackTrackConfig::H264 {
        width: 1920,
        height: 1080,
        nal_length_size: 3,
        sps: vec![vec![0x67]],
        pps: vec![vec![0x68]],
    };
    assert!(matches!(
        NativeVideoCapability::inspect(&forged),
        Err(AnnexBError::InvalidNalLengthSize(3))
    ));
}

#[test]
fn advertises_hevc_and_av1_as_typed_unsupported_capabilities() {
    let hevc = PlaybackTrackConfig::Hevc {
        width: 1920,
        height: 1080,
        nal_length_size: 4,
        vps: vec![vec![1]],
        sps: vec![vec![2]],
        pps: vec![vec![3]],
    };
    let av1 = PlaybackTrackConfig::Av1 {
        width: 1920,
        height: 1080,
        sequence_header_obu: vec![1],
    };

    assert_eq!(
        NativeVideoCapability::inspect(&hevc).unwrap(),
        NativeVideoCapability::Unsupported(UnsupportedVideoCodec::Hevc)
    );
    assert_eq!(
        NativeVideoCapability::inspect(&av1).unwrap(),
        NativeVideoCapability::Unsupported(UnsupportedVideoCodec::Av1)
    );
}

#[test]
fn preflight_rejects_sample_and_parameter_sets_before_transport_buffer_allocation() {
    let mut track = TrackIndex {
        id: 1,
        timescale: 60_000,
        config: PlaybackTrackConfig::H264 {
            width: 1920,
            height: 1080,
            nal_length_size: 4,
            sps: vec![vec![0x67]],
            pps: vec![vec![0x68]],
        },
        samples: vec![SampleIndex {
            offset: 0,
            size: 65,
            dts: 0,
            pts: 0,
            duration: 1_000,
            is_sync: true,
        }],
    };
    let limits = AnnexBLimits::new(64, 128).unwrap();
    assert!(matches!(
        plan_video_sample_buffers(&track, limits),
        Err(AnnexBError::EncodedSampleTooLarge {
            size: 65,
            limit: 64
        })
    ));

    track.samples[0].size = 1;
    track.config = PlaybackTrackConfig::H264 {
        width: 1920,
        height: 1080,
        nal_length_size: 4,
        sps: vec![vec![0x67; 9]],
        pps: vec![vec![0x68; 8]],
    };
    assert!(matches!(
        plan_video_sample_buffers(&track, AnnexBLimits::new(64, 24).unwrap()),
        Err(AnnexBError::OutputTooLarge {
            required: 25,
            limit: 24
        })
    ));
}

#[test]
fn production_fixture_is_byte_exact_bounded_and_allocation_stable_after_warmup() {
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/playback/hybrid-writer-h264-two-opus-5s.mp4");
    let movie = IndexedMovie::open(&fixture).unwrap();
    let mut oracle = IndexedMovie::open(&fixture).unwrap();
    let video_track_index = movie
        .index()
        .tracks
        .iter()
        .position(|track| matches!(track.config, PlaybackTrackConfig::H264 { .. }))
        .unwrap();
    let track = &movie.index().tracks[video_track_index];
    let sample_count = track.samples.len();
    let sync_count = track.samples.iter().filter(|sample| sample.is_sync).count();
    let max_sample_size = track
        .samples
        .iter()
        .map(|sample| sample.size as usize)
        .max()
        .unwrap();
    let (nal_length_size, sps, pps) = match &track.config {
        PlaybackTrackConfig::H264 {
            nal_length_size,
            sps,
            pps,
            ..
        } => (*nal_length_size as usize, sps.clone(), pps.clone()),
        _ => unreachable!(),
    };
    assert_eq!(sample_count, 150);
    assert_eq!(sync_count, 3);

    let mut transport =
        VideoSampleTransport::new(movie, video_track_index, OPEN_GENERATION).unwrap();
    let mut encoded = vec![0; max_sample_size];
    let mut first_generation_injections = 0;
    for sample_index in 0..sample_count {
        let metadata = oracle.index().tracks[video_track_index].samples[sample_index].clone();
        let read_size = oracle
            .read_sample_into(video_track_index, sample_index, &mut encoded)
            .unwrap();
        let mut expected = Vec::new();
        if metadata.is_sync && first_generation_injections == 0 {
            for parameter_set in sps.iter().chain(&pps) {
                expected.extend_from_slice(&[0, 0, 0, 1]);
                expected.extend_from_slice(parameter_set);
            }
        }
        expected.extend_from_slice(&convert_expected_sample(
            &encoded[..read_size],
            nal_length_size,
        ));

        let submission = {
            let unit = transport
                .read_sample(sample_index, OPEN_GENERATION)
                .unwrap();
            assert_eq!(unit.bytes, expected);
            assert_eq!(unit.encoded_size, metadata.size as usize);
            assert_eq!(unit.sample_index, sample_index);
            unit.parameter_set_submission
        };
        if let Some(submission) = submission {
            first_generation_injections += 1;
            assert!(transport.commit_parameter_sets(submission));
        }
    }
    assert_eq!(first_generation_injections, 1);
    let warmed = transport.buffer_telemetry();
    assert_eq!(warmed.encoded_reserve_count, 1);
    assert!(warmed.converted_reserve_count > 0);
    assert!(warmed.converted_reserve_count < sample_count);
    assert!(warmed.encoded_high_water <= warmed.encoded_logical_limit);
    assert!(warmed.converted_high_water <= warmed.converted_logical_limit);

    transport.reset_for_generation(SEEK_GENERATION);
    let mut seek_generation_injections = 0;
    for sample_index in 0..sample_count {
        let submission = transport
            .read_sample(sample_index, SEEK_GENERATION)
            .unwrap()
            .parameter_set_submission;
        if let Some(submission) = submission {
            seek_generation_injections += 1;
            assert!(transport.commit_parameter_sets(submission));
        }
    }
    assert_eq!(seek_generation_injections, 1);
    assert_eq!(transport.buffer_telemetry(), warmed);
}
