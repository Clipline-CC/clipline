#![cfg(windows)]

use std::time::{Duration, Instant};

use clipline_capture::clock::RelativeClock;
use clipline_capture::windows::qpc_now_ticks_100ns;
use clipline_capture::windows::wasapi::{WasapiChannelMode, WasapiMicrophoneMonitor};
use clipline_playback::windows::WindowsWasapiRenderer;

fn device_tests_disabled() -> bool {
    std::env::var_os("CI").is_some()
}

#[test]
fn default_microphone_monitor_is_bounded_and_records_endpoint_conversion_and_render_epoch() {
    if device_tests_disabled() {
        eprintln!("SKIP: Windows microphone device tests are disabled under CI");
        return;
    }

    let clock = match qpc_now_ticks_100ns() {
        Ok(origin) => RelativeClock::new(origin),
        Err(error) => {
            eprintln!("SKIP: QPC clock is unavailable: {error}");
            return;
        }
    };
    let mut monitor =
        match WasapiMicrophoneMonitor::start(clock, None, 1.0, WasapiChannelMode::Stereo) {
            Ok(monitor) => monitor,
            Err(error) => {
                eprintln!("SKIP: default microphone endpoint is unavailable: {error}");
                return;
            }
        };
    let info = monitor.info();
    assert!(info.endpoint_id().is_some_and(|id| !id.is_empty()));
    assert!(info.source_sample_rate > 0);
    assert!(info.source_channels > 0);
    assert_eq!(info.output_sample_rate, 48_000);
    assert_eq!(info.output_channels, 2);
    assert_eq!(info.maximum_samples_per_poll, 4_096);
    assert!(info.maximum_packet_frames > 0);
    assert!(info.decoded_scratch_capacity > 0);
    assert!(info.stereo_scratch_capacity > 0);
    assert!(info.resampled_scratch_capacity > 0);
    assert!(info.backlog_capacity >= 3_840);
    let scratch_capacities = (
        info.decoded_scratch_capacity,
        info.stereo_scratch_capacity,
        info.resampled_scratch_capacity,
        info.backlog_capacity,
    );

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let samples = monitor.poll_samples().expect("poll microphone samples");
        assert!(samples.len() <= info.maximum_samples_per_poll);
        assert!(samples.len().is_multiple_of(2));
        assert!(samples.iter().all(|sample| sample.is_finite()));
        if !samples.is_empty() || Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let after_polls = monitor.info();
    if after_polls.endpoint_id() == info.endpoint_id() {
        assert_eq!(
            (
                after_polls.decoded_scratch_capacity,
                after_polls.stereo_scratch_capacity,
                after_polls.resampled_scratch_capacity,
                after_polls.backlog_capacity,
            ),
            scratch_capacities,
            "steady monitor polls must not grow conversion or backlog storage"
        );
    }

    let renderer = match WindowsWasapiRenderer::open_default() {
        Ok(renderer) => renderer,
        Err(error) => {
            eprintln!(
                "SKIP: monitor capture passed, but default render endpoint telemetry is unavailable: {error}"
            );
            return;
        }
    };
    let renderer = renderer.telemetry();
    assert!(renderer.endpoint_epoch > 0);
    assert!(renderer.device_sample_rate > 0);
    assert!(renderer.device_channels > 0);
    println!(
        "microphone endpoint={:?} source={}Hz/{}ch/{} conversion={} renderer_endpoint={} renderer={}Hz/{}ch epoch={}",
        info.endpoint_id(),
        info.source_sample_rate,
        info.source_channels,
        info.source_format(),
        info.conversion_active,
        renderer.endpoint_id(),
        renderer.device_sample_rate,
        renderer.device_channels,
        renderer.endpoint_epoch,
    );
}
