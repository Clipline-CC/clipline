use super::*;
use crate::service::{
    AudioOptions, CaptureRegion, CaptureSource, ReplayStorageOptions, DEFAULT_DISK_QUOTA_BYTES,
};
use std::path::PathBuf;

#[test]
fn service_options_include_estimated_buffer_bytes() {
    let settings = AppSettings::default();
    let opts = settings
        .to_service_options(Some("http://mock".into()))
        .unwrap();

    assert_eq!(opts.replay_window_s, 60.0);
    assert_eq!(opts.fps, 60);
    assert_eq!(opts.bitrate_bps, 12_000_000);
    assert_eq!(opts.video_encoder, VideoEncoder::Auto);
    assert_eq!(opts.output_resolution, OutputResolution::Source);
    assert_eq!(opts.disk_quota_bytes, Some(DEFAULT_DISK_QUOTA_BYTES));
    assert_eq!(opts.media_dir, PathBuf::from(default_media_dir()));
    assert_eq!(opts.lol_url.as_deref(), Some("http://mock"));
    assert_eq!(opts.audio, AudioOptions::default());
    assert_eq!(opts.replay_storage, ReplayStorageOptions::Memory);
    assert_eq!(
        opts.buffer_bytes,
        estimated_buffer_bytes(60.0, settings.effective_bitrate_mbps())
    );
}

#[test]
fn service_options_include_audio_settings() {
    let settings = AppSettings {
        audio: AudioSettings {
            output_enabled: true,
            output_device_id: Some("output-id".into()),
            output_volume: 0.75,
            split_output_by_process: false,
            mic_enabled: true,
            mic_device_id: Some("mic-id".into()),
            mic_volume: 1.5,
            mic_channels: AudioChannelMode::Stereo,
        },
        ..AppSettings::default()
    };

    let opts = settings.to_service_options(None).unwrap();

    assert!(opts.audio.output_enabled);
    assert_eq!(opts.audio.output_device_id.as_deref(), Some("output-id"));
    assert_eq!(opts.audio.output_volume, 0.75);
    assert!(!opts.audio.split_output_by_process);
    assert!(opts.audio.mic_enabled);
    assert_eq!(opts.audio.mic_device_id.as_deref(), Some("mic-id"));
    assert_eq!(opts.audio.mic_volume, 1.5);
    assert_eq!(opts.audio.mic_channels, AudioChannelMode::Stereo);
}

#[test]
fn service_options_include_video_encoder_choice() {
    let settings = AppSettings {
        video_encoder: VideoEncoder::AmfH264,
        ..AppSettings::default()
    };

    let opts = settings.to_service_options(None).unwrap();

    assert_eq!(opts.video_encoder, VideoEncoder::AmfH264);
}

#[test]
fn service_options_include_capture_backend_choice() {
    let settings = AppSettings {
        capture_backend: CaptureBackend::DesktopDuplication,
        ..AppSettings::default()
    };

    let opts = settings.to_service_options(None).unwrap();

    assert_eq!(opts.capture_backend, CaptureBackend::DesktopDuplication);
}

#[test]
fn service_options_include_output_resolution_choice() {
    let settings = AppSettings {
        output_resolution: OutputResolution::P720,
        video_quality: VideoQuality::Sharp,
        ..AppSettings::default()
    };

    let opts = settings.to_service_options(None).unwrap();

    assert_eq!(opts.output_resolution, OutputResolution::P720);
    assert_eq!(opts.bitrate_bps, 8_000_000);
}

#[test]
fn advanced_recording_overrides_preset_service_values() {
    let settings = AppSettings {
        advanced_recording: AdvancedRecordingSettings {
            enabled: true,
            output_width: 1600,
            output_height: 900,
            bitrate_mbps: 13.5,
            fps: 75,
        },
        output_resolution: OutputResolution::P720,
        video_quality: VideoQuality::Compact,
        bitrate_mbps: 2.5,
        fps: 30,
        ..AppSettings::default()
    };

    let opts = settings.to_service_options(None).unwrap();
    let bounds = opts.output_resolution_bounds.unwrap();

    assert_eq!(opts.output_resolution, OutputResolution::P720);
    assert_eq!(bounds.width, 1600);
    assert_eq!(bounds.height, 900);
    assert_eq!(opts.bitrate_bps, 13_500_000);
    assert_eq!(opts.fps, 75);
}

#[test]
fn service_options_include_display_region_source() {
    let settings = AppSettings {
        capture_mode: CaptureMode::DisplayRegion,
        capture_region: CaptureRegionSettings {
            display_id: Some(r"\\.\DISPLAY1".into()),
            x: 100,
            y: 50,
            width: 800,
            height: 450,
        },
        ..AppSettings::default()
    };

    let opts = settings.to_service_options(None).unwrap();

    assert_eq!(
        opts.capture_source,
        CaptureSource::DisplayRegion(CaptureRegion {
            display_id: Some(r"\\.\DISPLAY1".into()),
            x: 100,
            y: 50,
            width: 800,
            height: 450,
        })
    );
}

#[test]
fn buffer_estimate_scales_with_duration_and_bitrate() {
    let small = estimated_buffer_bytes(60.0, 8.0);
    let large = estimated_buffer_bytes(120.0, 16.0);

    assert!(small >= 64 * 1024 * 1024);
    assert!(large > small * 3);
}

#[test]
fn thirty_second_replay_has_buffer_slack_for_encoder_overshoot() {
    let settings = AppSettings {
        replay_window_s: 30.0,
        buffer_seconds: 45.0,
        bitrate_mbps: 5.0,
        ..AppSettings::default()
    };
    let opts = settings.to_service_options(None).unwrap();

    assert!(opts.buffer_bytes >= 64 * 1024 * 1024);
}

#[test]
fn service_options_ignore_stale_legacy_buffer_seconds() {
    // `buffer_seconds` is a persisted compatibility mirror. Runtime sizing and
    // retention must derive from the replay window even if a live value is
    // stale or outside the old validation range.
    let settings = AppSettings {
        replay_window_s: 30.0,
        buffer_seconds: 1.0,
        ..AppSettings::default()
    };

    let opts = settings.to_service_options(None).unwrap();

    assert_eq!(opts.replay_window_s, 30.0);
    assert_eq!(
        opts.buffer_bytes,
        estimated_buffer_bytes(30.0, settings.effective_bitrate_mbps())
    );
}

#[test]
fn five_second_replay_uses_exact_retention() {
    let settings = AppSettings {
        replay_window_s: 5.0,
        buffer_seconds: 5.0,
        ..AppSettings::default()
    };

    let opts = settings.to_service_options(None).unwrap();

    assert_eq!(opts.replay_window_s, 5.0);
    assert_eq!(
        opts.buffer_bytes,
        estimated_buffer_bytes(5.0, settings.effective_bitrate_mbps())
    );
}
