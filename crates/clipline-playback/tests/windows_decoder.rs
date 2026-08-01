#![cfg(windows)]

use std::collections::BTreeSet;
use std::fs::File;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use clipline_mp4::{IndexedMovie, PlaybackTrackConfig};
use clipline_playback::windows::{
    classify_device_failure, probe_h264_decoders, DecoderPreference, WindowsH264Decoder,
};
use clipline_playback::{
    plan_video_sample_buffers, BackendComponent, BackendError, BackendErrorKind,
    EncodedVideoPacket, PipelineToken, RecoveryDisposition, SubmitStatus, TimelineDuration,
    TimelinePosition, VideoAcceleration, VideoDecoder, VideoPixelFormat, VideoSampleTransport,
    WorkGeneration, PLAYBACK_TIMELINE_HZ,
};
use windows::Win32::Graphics::Dxgi::{
    DXGI_ERROR_DEVICE_HUNG, DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_RESET,
    DXGI_ERROR_DRIVER_INTERNAL_ERROR,
};

const OPEN_GENERATION: WorkGeneration = WorkGeneration::new(1, 0);
const SEEK_GENERATION: WorkGeneration = WorkGeneration::new(1, 1);
const REOPEN_GENERATION: WorkGeneration = WorkGeneration::new(2, 0);
const OPEN_TOKEN: PipelineToken = PipelineToken::new(OPEN_GENERATION, 0);
const SEEK_TOKEN: PipelineToken = PipelineToken::new(SEEK_GENERATION, 1);
const REOPEN_TOKEN: PipelineToken = PipelineToken::new(REOPEN_GENERATION, 0);
const DEVICE_TIMEOUT: Duration = Duration::from_secs(10);

struct FixtureVideo {
    transport: VideoSampleTransport<File>,
    config: clipline_playback::H264DecoderConfig,
    timescale: u32,
    sample_count: usize,
    sync_samples: Vec<usize>,
}

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/playback/hybrid-writer-h264-two-opus-5s.mp4")
}

fn fixture_video(generation: WorkGeneration) -> FixtureVideo {
    let movie = IndexedMovie::open(fixture_path()).expect("open production-writer fixture");
    let video_track_index = movie
        .index()
        .tracks
        .iter()
        .position(|track| matches!(track.config, PlaybackTrackConfig::H264 { .. }))
        .expect("fixture H.264 track");
    let track = &movie.index().tracks[video_track_index];
    let plan = plan_video_sample_buffers(track, Default::default()).expect("fixture H.264 config");
    let timescale = track.timescale;
    let sample_count = track.samples.len();
    let sync_samples = track
        .samples
        .iter()
        .enumerate()
        .filter_map(|(index, sample)| sample.is_sync.then_some(index))
        .collect();

    FixtureVideo {
        transport: VideoSampleTransport::new(movie, video_track_index, generation)
            .expect("fixture video transport"),
        config: plan.config,
        timescale,
        sample_count,
        sync_samples,
    }
}

fn device_tests_disabled() -> bool {
    std::env::var_os("CI").is_some()
}

fn new_decoder_or_skip(preference: DecoderPreference) -> Option<WindowsH264Decoder> {
    if device_tests_disabled() {
        eprintln!("SKIP: Windows playback device tests are disabled under CI");
        return None;
    }
    match WindowsH264Decoder::new(preference) {
        Ok(decoder) => Some(decoder),
        Err(error) => {
            eprintln!("SKIP: Media Foundation H.264 decoding unavailable: {error}");
            None
        }
    }
}

fn timeline_position(pts: i64, timescale: u32) -> TimelinePosition {
    let pts = u64::try_from(pts).expect("fixture video PTS is non-negative");
    TimelinePosition::new(
        (u128::from(pts) * u128::from(PLAYBACK_TIMELINE_HZ) / u128::from(timescale)) as u64,
    )
}

fn timeline_duration(duration: u32, timescale: u32) -> TimelineDuration {
    let ticks = u128::from(duration) * u128::from(PLAYBACK_TIMELINE_HZ) / u128::from(timescale);
    TimelineDuration::new(u64::try_from(ticks).expect("fixture duration fits u64"))
        .expect("fixture frame has non-zero duration")
}

fn receive_available(
    decoder: &mut WindowsH264Decoder,
    token: PipelineToken,
    decoded_indices: &mut BTreeSet<usize>,
) -> Result<usize, BackendError> {
    let mut received = 0;
    while let Some(frame) = decoder.receive()? {
        assert_eq!(
            frame.token(),
            token,
            "decoder must retain submit-time token"
        );
        assert!(
            decoded_indices.insert(frame.sample_index()),
            "decoder returned sample {} more than once",
            frame.sample_index()
        );
        received += 1;

        // A returned frame is already presentable. At this exact boundary the
        // transform's sample must have been released and its texture copied to
        // playback-owned storage, otherwise the MFT surface pool can exhaust.
        let ownership = decoder.ownership_telemetry();
        assert_eq!(ownership.output_copies, ownership.presentable_frames);
        assert!(ownership.mft_samples_released >= ownership.presentable_frames);
        assert!(ownership.mft_samples_received >= ownership.mft_samples_released);
    }
    Ok(received)
}

fn submit_fixture_sample(
    fixture: &mut FixtureVideo,
    decoder: &mut WindowsH264Decoder,
    sample_index: usize,
    generation: WorkGeneration,
    token: PipelineToken,
    decoded_indices: &mut BTreeSet<usize>,
) -> Result<(), BackendError> {
    let deadline = Instant::now() + DEVICE_TIMEOUT;
    loop {
        let submission = {
            let unit = fixture
                .transport
                .read_sample(sample_index, generation)
                .expect("read and convert fixture access unit");
            let status = decoder.submit(
                EncodedVideoPacket {
                    bytes: unit.bytes,
                    sample_index: unit.sample_index,
                    pts: timeline_position(unit.pts, fixture.timescale),
                    duration: timeline_duration(unit.duration, fixture.timescale),
                    is_sync: unit.is_sync,
                },
                token,
            )?;
            (status, unit.parameter_set_submission)
        };

        match submission.0 {
            SubmitStatus::Accepted => {
                if let Some(parameter_sets) = submission.1 {
                    assert!(fixture.transport.commit_parameter_sets(parameter_sets));
                }
                break;
            }
            SubmitStatus::Backpressured => {
                receive_available(decoder, token, decoded_indices)?;
                assert!(
                    Instant::now() < deadline,
                    "decoder stayed backpressured for {DEVICE_TIMEOUT:?}"
                );
                std::thread::yield_now();
            }
        }
    }
    receive_available(decoder, token, decoded_indices)?;
    Ok(())
}

fn drain_to_count(
    decoder: &mut WindowsH264Decoder,
    token: PipelineToken,
    decoded_indices: &mut BTreeSet<usize>,
    expected: usize,
) -> Result<(), BackendError> {
    decoder.drain(token)?;
    let deadline = Instant::now() + DEVICE_TIMEOUT;
    while decoded_indices.len() < expected && Instant::now() < deadline {
        if receive_available(decoder, token, decoded_indices)? == 0 {
            std::thread::yield_now();
        }
    }
    assert_eq!(decoded_indices.len(), expected, "decoder frame count");
    Ok(())
}

#[test]
fn capability_probe_reports_the_software_fallback_and_exact_selected_path() {
    if device_tests_disabled() {
        eprintln!("SKIP: Windows playback device tests are disabled under CI");
        return;
    }
    let capabilities = match probe_h264_decoders() {
        Ok(capabilities) => capabilities,
        Err(error) => {
            eprintln!("SKIP: Media Foundation H.264 decoding unavailable: {error}");
            return;
        }
    };

    if !capabilities.software_available() {
        eprintln!("SKIP: inbox software H.264 decoder is unavailable");
        return;
    }

    let mut fixture = fixture_video(OPEN_GENERATION);
    let mut decoder = WindowsH264Decoder::new(DecoderPreference::SoftwareOnly)
        .expect("construct software H.264 decoder backend");
    assert_eq!(decoder.preference(), DecoderPreference::SoftwareOnly);
    decoder
        .configure(&fixture.config, OPEN_TOKEN)
        .expect("configure software decoder");
    let info = decoder.info().expect("configured decoder info");
    assert_eq!(info.acceleration, VideoAcceleration::Software);
    assert_eq!(info.pixel_format, VideoPixelFormat::Nv12);
    assert_eq!((info.width, info.height), (640, 360));

    // Keep the fixture alive until after configuration so this assertion also
    // catches a backend that retained borrowed decoder configuration data.
    fixture.transport.reset_for_generation(SEEK_GENERATION);
}

#[test]
fn production_fixture_decodes_every_frame_to_owned_nv12_surfaces() {
    let Some(mut decoder) = new_decoder_or_skip(DecoderPreference::PreferHardware) else {
        return;
    };
    let mut fixture = fixture_video(OPEN_GENERATION);
    decoder
        .configure(&fixture.config, OPEN_TOKEN)
        .expect("configure H.264 decoder");
    let info = decoder.info().expect("configured decoder info");
    assert_eq!((info.width, info.height), (640, 360));
    assert_eq!(info.pixel_format, VideoPixelFormat::Nv12);
    assert!(matches!(
        info.acceleration,
        VideoAcceleration::Hardware | VideoAcceleration::Software
    ));

    let mut decoded_indices = BTreeSet::new();
    for sample_index in 0..fixture.sample_count {
        submit_fixture_sample(
            &mut fixture,
            &mut decoder,
            sample_index,
            OPEN_GENERATION,
            OPEN_TOKEN,
            &mut decoded_indices,
        )
        .expect("decode fixture access unit");
    }
    drain_to_count(
        &mut decoder,
        OPEN_TOKEN,
        &mut decoded_indices,
        fixture.sample_count,
    )
    .expect("drain fixture decoder");

    assert_eq!(decoded_indices.first().copied(), Some(0));
    assert_eq!(
        decoded_indices.last().copied(),
        Some(fixture.sample_count - 1)
    );
    let ownership = decoder.ownership_telemetry();
    assert_eq!(ownership.presentable_frames, fixture.sample_count as u64);
    assert_eq!(ownership.output_copies, ownership.presentable_frames);
    assert_eq!(
        ownership.mft_samples_released, ownership.mft_samples_received,
        "all MFT samples must be released after drain"
    );
    decoder.close();
}

#[test]
fn flush_fences_old_output_and_decoder_can_be_reopened() {
    let Some(mut decoder) = new_decoder_or_skip(DecoderPreference::PreferHardware) else {
        return;
    };
    let mut fixture = fixture_video(OPEN_GENERATION);
    decoder
        .configure(&fixture.config, OPEN_TOKEN)
        .expect("configure first decoder generation");
    let mut decoded = BTreeSet::new();
    for sample_index in 0..8 {
        submit_fixture_sample(
            &mut fixture,
            &mut decoder,
            sample_index,
            OPEN_GENERATION,
            OPEN_TOKEN,
            &mut decoded,
        )
        .expect("decode before flush");
    }

    decoder.flush(SEEK_TOKEN).expect("flush decoder for seek");
    assert!(
        decoder.receive().expect("receive after flush").is_none(),
        "flush must discard all queued output from the prior token"
    );

    fixture.transport.reset_for_generation(SEEK_GENERATION);
    let seek_sync = fixture.sync_samples[1];
    let mut sought = BTreeSet::new();
    submit_fixture_sample(
        &mut fixture,
        &mut decoder,
        seek_sync,
        SEEK_GENERATION,
        SEEK_TOKEN,
        &mut sought,
    )
    .expect("decode after flush");
    drain_to_count(&mut decoder, SEEK_TOKEN, &mut sought, 1).expect("drain after flush");
    assert_eq!(sought.first().copied(), Some(seek_sync));
    decoder.close();

    let Some(mut reopened) = new_decoder_or_skip(decoder.preference()) else {
        return;
    };
    let mut reopened_fixture = fixture_video(REOPEN_GENERATION);
    reopened
        .configure(&reopened_fixture.config, REOPEN_TOKEN)
        .expect("configure reopened decoder");
    let mut reopened_frames = BTreeSet::new();
    submit_fixture_sample(
        &mut reopened_fixture,
        &mut reopened,
        0,
        REOPEN_GENERATION,
        REOPEN_TOKEN,
        &mut reopened_frames,
    )
    .expect("decode after reopen");
    drain_to_count(&mut reopened, REOPEN_TOKEN, &mut reopened_frames, 1)
        .expect("drain reopened decoder");
    assert_eq!(reopened_frames.first().copied(), Some(0));
    reopened.close();
}

#[test]
fn corrupt_annex_b_access_unit_is_a_typed_non_device_failure() {
    let Some(mut decoder) = new_decoder_or_skip(DecoderPreference::PreferHardware) else {
        return;
    };
    let fixture = fixture_video(OPEN_GENERATION);
    decoder
        .configure(&fixture.config, OPEN_TOKEN)
        .expect("configure decoder");

    let error = decoder
        .submit(
            EncodedVideoPacket {
                // Deliberately lacks an Annex-B start code. Rejecting it before
                // ProcessInput keeps corrupt recovery deterministic across MFTs.
                bytes: &[0x67, 0x64, 0x00, 0x1f],
                sample_index: 0,
                pts: TimelinePosition::new(0),
                duration: TimelineDuration::new(1_600).unwrap(),
                is_sync: true,
            },
            OPEN_TOKEN,
        )
        .expect_err("invalid Annex-B access unit must be rejected");
    assert_eq!(error.component, BackendComponent::VideoDecoder);
    assert_eq!(error.kind, BackendErrorKind::CorruptInput);
    assert_eq!(error.recovery, RecoveryDisposition::RetryPipeline);
    assert_eq!(decoder.ownership_telemetry().presentable_frames, 0);
}

#[test]
fn explicit_software_fallback_is_labeled_in_decoder_info() {
    if device_tests_disabled() {
        eprintln!("SKIP: Windows playback device tests are disabled under CI");
        return;
    }
    let capabilities = match probe_h264_decoders() {
        Ok(capabilities) => capabilities,
        Err(error) => {
            eprintln!("SKIP: Media Foundation H.264 decoding unavailable: {error}");
            return;
        }
    };
    if !capabilities.software_available() {
        eprintln!("SKIP: inbox software H.264 decoder is unavailable");
        return;
    }

    let Some(mut decoder) = new_decoder_or_skip(DecoderPreference::SoftwareOnly) else {
        return;
    };
    let fixture = fixture_video(OPEN_GENERATION);
    decoder
        .configure(&fixture.config, OPEN_TOKEN)
        .expect("configure software decoder");
    assert_eq!(decoder.preference(), DecoderPreference::SoftwareOnly);
    assert_eq!(
        decoder
            .info()
            .expect("configured decoder info")
            .acceleration,
        VideoAcceleration::Software
    );
}

#[test]
fn dxgi_device_loss_hresult_values_request_component_recreation() {
    for hresult in [
        DXGI_ERROR_DEVICE_REMOVED,
        DXGI_ERROR_DEVICE_RESET,
        DXGI_ERROR_DEVICE_HUNG,
        DXGI_ERROR_DRIVER_INTERNAL_ERROR,
    ] {
        let error = classify_device_failure(hresult.0);
        assert_eq!(error.component, BackendComponent::VideoDecoder);
        assert_eq!(error.kind, BackendErrorKind::DeviceLost);
        assert_eq!(error.recovery, RecoveryDisposition::RecreateComponent);
        assert_eq!(error.native_code, Some(i64::from(hresult.0)));
    }
}
