#![cfg(windows)]

use std::time::{Duration, Instant};

use clipline_playback::windows::{
    classify_audio_failure, WasapiInitializationPath, WindowsWasapiRenderer,
};
use clipline_playback::{
    AudioRenderer, AudioSampleFormat, BackendComponent, BackendError, BackendErrorKind,
    PipelineToken, RecoveryDisposition, WorkGeneration, MAX_AUDIO_WRITE_FRAMES,
    PLAYBACK_TIMELINE_HZ,
};
use windows::Win32::Media::Audio::{
    AUDCLNT_E_DEVICE_INVALIDATED, AUDCLNT_E_RESOURCES_INVALIDATED, AUDCLNT_E_SERVICE_NOT_RUNNING,
};

const OPEN_TOKEN: PipelineToken = PipelineToken::new(WorkGeneration::new(1, 0), 0);
const SEEK_TOKEN: PipelineToken = PipelineToken::new(WorkGeneration::new(1, 1), 1);
const REOPEN_TOKEN: PipelineToken = PipelineToken::new(WorkGeneration::new(2, 0), 0);
const CLOCK_PROGRESS_TIMEOUT: Duration = Duration::from_secs(1);
const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

fn device_tests_disabled() -> bool {
    std::env::var_os("CI").is_some()
}

fn open_renderer_or_skip() -> Option<WindowsWasapiRenderer> {
    if device_tests_disabled() {
        eprintln!("SKIP: Windows playback audio device tests are disabled under CI");
        return None;
    }

    match WindowsWasapiRenderer::open_default() {
        Ok(renderer) => Some(renderer),
        Err(error) => {
            eprintln!("SKIP: default WASAPI render endpoint is unavailable: {error}");
            None
        }
    }
}

fn wait_for_clock_progress(
    renderer: &mut WindowsWasapiRenderer,
    initial_position: u64,
) -> Result<u64, BackendError> {
    let deadline = Instant::now() + CLOCK_PROGRESS_TIMEOUT;
    loop {
        let position = renderer.raw_clock()?.position();
        if position > initial_position {
            return Ok(position);
        }
        assert!(
            Instant::now() < deadline,
            "IAudioClock did not advance within {CLOCK_PROGRESS_TIMEOUT:?}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn default_renderer_is_bounded_clocked_and_reopenable() {
    let Some(mut renderer) = open_renderer_or_skip() else {
        return;
    };

    renderer
        .reset(OPEN_TOKEN)
        .expect("reset newly opened endpoint");
    let info = renderer.info();
    let telemetry = renderer.telemetry();

    assert_eq!(info.sample_rate, PLAYBACK_TIMELINE_HZ);
    assert_eq!(info.channels, 2);
    assert_eq!(info.sample_format, AudioSampleFormat::F32);
    assert!(info.buffer_frames > 0, "WASAPI buffer must be non-empty");
    assert!(
        info.buffer_frames <= PLAYBACK_TIMELINE_HZ as usize,
        "shared endpoint buffer must remain bounded to at most one second"
    );
    assert_eq!(telemetry.buffer_frames, info.buffer_frames);
    assert_eq!(telemetry.endpoint_epoch, info.endpoint_epoch);
    assert!(!telemetry.endpoint_id().is_empty());
    assert!(!telemetry.device_format().is_empty());
    assert!(telemetry.device_sample_rate > 0);
    assert!(telemetry.device_channels > 0);
    assert!(telemetry.device_bits_per_sample > 0);
    assert!(telemetry.device_buffer_duration_100ns > 0);
    if telemetry.initialization_path == WasapiInitializationPath::LegacySharedAutoConvert {
        assert!(telemetry.conversion_active);
    } else {
        eprintln!(
            "SKIP: default endpoint accepted IAudioClient3 at 48 kHz; legacy conversion fallback not exercised"
        );
    }
    if !telemetry.conversion_active {
        assert_eq!(telemetry.device_sample_rate, PLAYBACK_TIMELINE_HZ);
        assert_eq!(telemetry.device_channels, 2);
    }

    // Fill the stopped client in scheduler-sized chunks. A stopped endpoint has
    // stable padding, making the accepted-frame bound deterministic without
    // relying on wall-clock timing.
    let initial_writable = renderer
        .writable_frames()
        .expect("query initial endpoint padding");
    assert!(initial_writable > 0);
    assert!(initial_writable <= info.buffer_frames);
    let stale_error = renderer
        .write_stereo_frames(&[0.25, 0.25], SEEK_TOKEN)
        .expect_err("stale generation must not write endpoint data");
    assert_eq!(stale_error.component, BackendComponent::AudioRenderer);
    assert_eq!(stale_error.kind, BackendErrorKind::StaleWork);
    assert_eq!(stale_error.recovery, RecoveryDisposition::RetryPipeline);
    assert_eq!(
        renderer
            .writable_frames()
            .expect("stale write leaves endpoint padding unchanged"),
        initial_writable
    );
    let mut total_accepted = 0usize;
    for _ in 0..64 {
        let writable = renderer.writable_frames().expect("query endpoint padding");
        assert!(writable <= info.buffer_frames);
        if writable == 0 {
            break;
        }
        let requested = writable.min(MAX_AUDIO_WRITE_FRAMES);
        let pcm = vec![0.125_f32; requested * 2];
        let accepted = renderer
            .write_stereo_frames(&pcm, OPEN_TOKEN)
            .expect("write bounded stereo PCM");
        assert!(accepted <= requested);
        assert!(accepted <= writable);
        assert!(accepted > 0, "a writable stopped endpoint must accept PCM");
        total_accepted += accepted;
    }
    assert_eq!(total_accepted, initial_writable);
    assert_eq!(
        renderer
            .writable_frames()
            .expect("query full endpoint padding"),
        0
    );
    assert_eq!(
        renderer
            .write_stereo_frames(&[0.0, 0.0], OPEN_TOKEN)
            .expect("a full endpoint reports non-blocking backpressure"),
        0
    );
    let filled = renderer.telemetry();
    assert_eq!(filled.frames_written, total_accepted as u64);
    assert!(filled.max_frames_written_per_call <= MAX_AUDIO_WRITE_FRAMES);
    assert!(filled.max_frames_written_per_call <= info.buffer_frames);

    // Valid volume changes must not disturb the endpoint. Invalid values are
    // rejected at this safe boundary rather than reaching ISimpleAudioVolume.
    for volume in [0.0, 0.25, 1.0] {
        renderer.set_volume(volume).expect("set valid volume");
    }
    for volume in [-0.01, 1.01, f32::NAN] {
        let error = renderer
            .set_volume(volume)
            .expect_err("reject invalid volume");
        assert_eq!(error.component, BackendComponent::AudioRenderer);
        assert_eq!(error.recovery, RecoveryDisposition::Fatal);
    }
    renderer.set_volume(1.0).expect("restore unity volume");

    let before_start = renderer.raw_clock().expect("clock before start");
    assert!(before_start.frequency() > 0);
    assert_eq!(before_start.endpoint_epoch(), info.endpoint_epoch);
    renderer.start(OPEN_TOKEN).expect("start render endpoint");
    renderer
        .start(OPEN_TOKEN)
        .expect("duplicate play intent is idempotent");
    let playing_position = wait_for_clock_progress(&mut renderer, before_start.position())
        .expect("sample progressing IAudioClock");

    renderer.pause(OPEN_TOKEN).expect("pause render endpoint");
    renderer
        .pause(OPEN_TOKEN)
        .expect("duplicate pause intent is idempotent");
    let paused = renderer.raw_clock().expect("clock immediately after pause");
    assert!(paused.position() >= playing_position);
    std::thread::sleep(Duration::from_millis(40));
    let still_paused = renderer.raw_clock().expect("clock while paused");
    assert_eq!(still_paused.position(), paused.position());
    assert_eq!(still_paused.frequency(), paused.frequency());
    assert_eq!(still_paused.endpoint_epoch(), paused.endpoint_epoch());

    // Once real PCM has exhausted, WASAPI renders silence. The renderer
    // records one latched underrun episode rather than inflating the counter
    // on every empty-padding poll.
    renderer.reset(SEEK_TOKEN).expect("reset for underrun test");
    let short_frames = (PLAYBACK_TIMELINE_HZ as usize / 200).min(info.buffer_frames);
    let short_pcm = vec![0.0_f32; short_frames * 2];
    assert_eq!(
        renderer
            .write_stereo_frames(&short_pcm, SEEK_TOKEN)
            .expect("queue short underrun vector"),
        short_frames
    );
    renderer.start(SEEK_TOKEN).expect("start underrun vector");
    let underrun_deadline = Instant::now() + CLOCK_PROGRESS_TIMEOUT;
    loop {
        if renderer
            .writable_frames()
            .expect("poll short underrun vector")
            == info.buffer_frames
        {
            break;
        }
        assert!(
            Instant::now() < underrun_deadline,
            "endpoint did not exhaust the short underrun vector"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    let first_underrun = renderer.telemetry();
    assert_eq!(first_underrun.underruns, filled.underruns + 1);
    assert!(first_underrun.underrun_frames > filled.underrun_frames);
    assert_eq!(
        renderer
            .writable_frames()
            .expect("poll already-latched underrun"),
        info.buffer_frames
    );
    let latched_underrun = renderer.telemetry();
    assert_eq!(latched_underrun.underruns, first_underrun.underruns);
    assert_eq!(
        latched_underrun.underrun_frames,
        first_underrun.underrun_frames
    );
    renderer.pause(SEEK_TOKEN).expect("pause underrun vector");

    // Reset is the seek/flush boundary. The scheduler rebases this raw device
    // position to SEEK_TOKEN's settled target; the endpoint identity remains.
    renderer.reset(SEEK_TOKEN).expect("reset endpoint for seek");
    let reset_clock = renderer.raw_clock().expect("clock after seek reset");
    assert_eq!(reset_clock.endpoint_epoch(), info.endpoint_epoch);
    assert_eq!(
        renderer
            .writable_frames()
            .expect("query writable frames after reset"),
        info.buffer_frames
    );

    let drain_frames = (PLAYBACK_TIMELINE_HZ as usize / 100).min(info.buffer_frames);
    let drain_pcm = vec![0.0_f32; drain_frames * 2];
    assert_eq!(
        renderer
            .write_stereo_frames(&drain_pcm, SEEK_TOKEN)
            .expect("queue PCM for drain"),
        drain_frames
    );
    renderer.start(SEEK_TOKEN).expect("start drain playback");
    assert!(
        renderer
            .drain(SEEK_TOKEN, DRAIN_TIMEOUT)
            .expect("bounded endpoint drain"),
        "endpoint padding must reach zero within {DRAIN_TIMEOUT:?}"
    );
    renderer.pause(SEEK_TOKEN).expect("pause after drain");
    assert_eq!(
        renderer
            .writable_frames()
            .expect("query endpoint after drain"),
        info.buffer_frames
    );

    let epoch_before_reopen = renderer.info().endpoint_epoch;
    let before_reopen = renderer.telemetry();
    renderer.close();
    renderer.close();
    let closed_error = renderer
        .writable_frames()
        .expect_err("closed endpoint must reject rendering calls");
    assert_eq!(closed_error.component, BackendComponent::AudioRenderer);
    assert_eq!(closed_error.kind, BackendErrorKind::Unavailable);

    renderer
        .reopen(REOPEN_TOKEN)
        .expect("reopen default render endpoint");
    renderer
        .reset(REOPEN_TOKEN)
        .expect("reset reopened endpoint");
    let reopened = renderer.info();
    let reopened_telemetry = renderer.telemetry();
    assert!(reopened.endpoint_epoch > epoch_before_reopen);
    assert_eq!(reopened_telemetry.endpoint_epoch, reopened.endpoint_epoch);
    assert_eq!(reopened_telemetry.recovery_count, 1);
    assert_eq!(reopened_telemetry.buffer_frames, reopened.buffer_frames);
    assert_eq!(reopened_telemetry.underruns, before_reopen.underruns);
    assert_eq!(
        reopened_telemetry.underrun_frames,
        before_reopen.underrun_frames
    );
    assert_eq!(
        renderer
            .raw_clock()
            .expect("clock after endpoint recreation")
            .endpoint_epoch(),
        reopened.endpoint_epoch
    );
    renderer.close();
}

#[test]
fn endpoint_invalidation_hresult_values_request_renderer_recreation() {
    for hresult in [
        AUDCLNT_E_DEVICE_INVALIDATED,
        AUDCLNT_E_RESOURCES_INVALIDATED,
        AUDCLNT_E_SERVICE_NOT_RUNNING,
    ] {
        let error = classify_audio_failure(hresult.0);
        assert_eq!(error.component, BackendComponent::AudioRenderer);
        assert_eq!(error.kind, BackendErrorKind::EndpointInvalidated);
        assert_eq!(error.recovery, RecoveryDisposition::RecreateComponent);
        assert_eq!(error.native_code, Some(i64::from(hresult.0)));
    }
}
