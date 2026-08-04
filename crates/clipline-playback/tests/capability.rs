use clipline_playback::{
    H264PlaybackSupport, LimitedNativePlayback, PlaybackCapabilities, PlaybackCodec,
    PlaybackSupport,
};

#[test]
fn configured_hardware_software_and_unavailable_are_distinct() {
    let hardware =
        PlaybackCapabilities::new(Some(0x1122_3344), H264PlaybackSupport::ConfiguredHardware);
    let software =
        PlaybackCapabilities::new(Some(0x1122_3344), H264PlaybackSupport::ConfiguredSoftware);
    let unavailable = PlaybackCapabilities::new(None, H264PlaybackSupport::Unavailable);

    assert_eq!(
        hardware.support(PlaybackCodec::H264),
        PlaybackSupport::Hardware
    );
    assert_eq!(
        software.support(PlaybackCodec::H264),
        PlaybackSupport::Software
    );
    assert_eq!(
        unavailable.support(PlaybackCodec::H264),
        PlaybackSupport::Unavailable
    );
    assert!(hardware.native_decodable(PlaybackCodec::H264));
    assert!(software.native_decodable(PlaybackCodec::H264));
    assert!(!unavailable.native_decodable(PlaybackCodec::H264));
}

#[test]
fn hevc_and_av1_stay_explicitly_limited_until_their_native_gates_pass() {
    let capabilities = PlaybackCapabilities::new(None, H264PlaybackSupport::ConfiguredSoftware);

    assert_eq!(capabilities.hevc, LimitedNativePlayback::Ungated);
    assert_eq!(capabilities.av1, LimitedNativePlayback::Ungated);
    assert_eq!(
        capabilities.support(PlaybackCodec::Hevc),
        PlaybackSupport::LimitedNativePlayback
    );
    assert_eq!(
        capabilities.support(PlaybackCodec::Av1),
        PlaybackSupport::LimitedNativePlayback
    );
    assert!(!capabilities.native_decodable(PlaybackCodec::Hevc));
    assert!(!capabilities.native_decodable(PlaybackCodec::Av1));
}

#[test]
fn adapter_identity_and_typed_statuses_are_serialized_without_unbounded_details() {
    let json = serde_json::to_value(PlaybackCapabilities::new(
        Some(9),
        H264PlaybackSupport::ConfiguredHardware,
    ))
    .unwrap();
    assert_eq!(json["adapter_luid"], 9);
    assert_eq!(json["h264"], "configured_hardware");
    assert_eq!(json["hevc"], "ungated");
    assert_eq!(json["av1"], "ungated");
}

#[cfg(windows)]
#[test]
fn windows_probe_configures_a_real_h264_decoder_and_releases_the_session() {
    if std::env::var_os("CI").is_some() {
        eprintln!("SKIP: Windows playback device probes are disabled under CI");
        return;
    }
    let mut activation_checkpoints = 0usize;
    let capabilities =
        clipline_playback::windows::probe_playback_capabilities_with_checkpoint(|| {
            activation_checkpoints += 1;
            Ok(())
        })
        .expect("configure a native H.264 decoder capability");

    assert!(activation_checkpoints >= 1);
    assert!(activation_checkpoints <= 2);
    assert!(matches!(
        capabilities.h264,
        H264PlaybackSupport::ConfiguredHardware | H264PlaybackSupport::ConfiguredSoftware
    ));

    // A second probe must be able to configure immediately: the first probe
    // retained no decoder session, transform, device manager, or surface.
    let refreshed = clipline_playback::windows::probe_playback_capabilities()
        .expect("refresh configured native H.264 capability");
    assert_eq!(refreshed.adapter_luid, capabilities.adapter_luid);
}

#[cfg(windows)]
#[test]
fn capability_probe_never_constructs_a_playback_session_or_surface_pool() {
    let source = include_str!("../src/windows/capability.rs");
    for forbidden in [
        "WindowsH264Decoder",
        "DecoderSession",
        "create_nv12_texture",
        "TexturePool",
    ] {
        assert!(
            !source.contains(forbidden),
            "capability probe must stop at configured-transform truth, not {forbidden}"
        );
    }
}
