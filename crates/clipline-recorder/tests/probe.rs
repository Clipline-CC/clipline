use clipline_recorder::probe::{
    validate_encoders, EncoderOption, MAX_PROBE_ENCODERS, MAX_PROBE_TEXT_BYTES,
};

fn encoder(index: usize) -> EncoderOption {
    EncoderOption {
        id: format!("encoder-{index}"),
        name: format!("Encoder {index}"),
        codec: "h264".into(),
    }
}

#[test]
fn encoder_catalog_enforces_count_and_utf8_bounds() {
    let valid: Vec<_> = (0..MAX_PROBE_ENCODERS).map(encoder).collect();
    validate_encoders(&valid).unwrap();

    let too_many: Vec<_> = (0..=MAX_PROBE_ENCODERS).map(encoder).collect();
    assert!(validate_encoders(&too_many).unwrap_err().contains("count"));

    let huge = vec![EncoderOption {
        id: "id".into(),
        name: "é".repeat(MAX_PROBE_TEXT_BYTES),
        codec: "h264".into(),
    }];
    assert!(validate_encoders(&huge).unwrap_err().contains("maximum"));
}

#[cfg(windows)]
#[test]
fn concrete_catalog_keeps_domain_dtos_and_exact_probe_kind() {
    use clipline_capture::windows::display::DisplayInfo;
    use clipline_recorder::SettingsProbeCatalog;
    use clipline_settings::{BoundedProbePayload, ProbeKind};

    let catalog = SettingsProbeCatalog::Displays(vec![DisplayInfo {
        id: r"\\.\DISPLAY1".into(),
        name: "DISPLAY1".into(),
        x: -1920,
        y: 0,
        width: 1920,
        height: 1080,
        is_primary: false,
    }]);
    assert_eq!(catalog.kind(), ProbeKind::Displays);
    catalog.validate_bounds().unwrap();
}

#[cfg(windows)]
#[test]
fn native_playback_truth_only_admits_configured_h264_to_automatic_recording() {
    use clipline_playback::{H264PlaybackSupport, PlaybackCapabilities};
    use clipline_recorder::{
        native_decodable_codecs, native_playback_warning, Codec, NativePlaybackWarning,
        ServiceOptions,
    };

    let hardware = PlaybackCapabilities::new(Some(7), H264PlaybackSupport::ConfiguredHardware);
    let software = PlaybackCapabilities::new(Some(7), H264PlaybackSupport::ConfiguredSoftware);
    let unavailable = PlaybackCapabilities::new(None, H264PlaybackSupport::Unavailable);

    assert_eq!(native_decodable_codecs(hardware), vec![Codec::H264]);
    assert_eq!(native_decodable_codecs(software), vec![Codec::H264]);
    assert!(native_decodable_codecs(unavailable).is_empty());
    assert_eq!(native_playback_warning(hardware, Codec::H264), None);
    assert_eq!(
        native_playback_warning(unavailable, Codec::H264),
        Some(NativePlaybackWarning::Unavailable)
    );
    assert_eq!(
        native_playback_warning(hardware, Codec::Hevc),
        Some(NativePlaybackWarning::LimitedNativePlayback)
    );
    assert_eq!(
        native_playback_warning(hardware, Codec::Av1),
        Some(NativePlaybackWarning::LimitedNativePlayback)
    );

    let mut options = ServiceOptions::default();
    options.use_native_playback_capabilities(unavailable);
    assert!(options.decodable_codecs.is_empty());
    options.use_native_playback_capabilities(hardware);
    assert_eq!(options.decodable_codecs, vec![Codec::H264]);
}

#[cfg(windows)]
#[test]
fn playback_capability_catalog_is_bounded_and_uses_the_exact_probe_kind() {
    use clipline_playback::{H264PlaybackSupport, PlaybackCapabilities};
    use clipline_recorder::SettingsProbeCatalog;
    use clipline_settings::{BoundedProbePayload, ProbeKind};

    let catalog = SettingsProbeCatalog::PlaybackCapabilities(PlaybackCapabilities::new(
        Some(11),
        H264PlaybackSupport::ConfiguredSoftware,
    ));
    assert_eq!(catalog.kind(), ProbeKind::PlaybackCapabilities);
    catalog.validate_bounds().unwrap();
}
