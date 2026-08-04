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
