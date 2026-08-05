use clipline_settings::{
    AppSettings, OsuAccountGeneration, OsuAccountGenerationError, OsuApiSettings,
    MAX_OSU_CLIENT_ID_DIGITS, MAX_OSU_CONNECTED_USERNAME_BYTES, MAX_OSU_CREDENTIAL_CLEANUP_TARGETS,
    MAX_OSU_CREDENTIAL_TARGET_BYTES, MAX_OSU_PROFILE_BYTES, MAX_OSU_USER_BYTES,
};

fn repeated_ascii(byte: u8, len: usize) -> String {
    String::from_utf8(vec![byte; len]).unwrap()
}

fn cleanup_target(index: usize, len: usize) -> String {
    let prefix = format!("Clipline osu!:{index:02}:");
    format!("{prefix}{}", repeated_ascii(b'x', len - prefix.len()))
}

#[test]
fn exact_osu_profile_boundaries_validate() {
    let settings = OsuApiSettings {
        account_generation: OsuAccountGeneration::INITIAL,
        client_id: Some(u64::MAX.to_string()),
        user: Some(repeated_ascii(b'u', MAX_OSU_USER_BYTES)),
        credential_target: Some(cleanup_target(99, MAX_OSU_CREDENTIAL_TARGET_BYTES)),
        credential_cleanup_targets: (0..MAX_OSU_CREDENTIAL_CLEANUP_TARGETS)
            .map(|index| {
                cleanup_target(
                    index,
                    if index + 1 == MAX_OSU_CREDENTIAL_CLEANUP_TARGETS {
                        3_818
                    } else {
                        3_806
                    },
                )
            })
            .collect(),
        last_connected_username: Some(repeated_ascii(b'n', MAX_OSU_CONNECTED_USERNAME_BYTES)),
    };

    settings.validate().unwrap();
}

#[test]
fn osu_text_limits_count_utf8_bytes_instead_of_scalar_values() {
    let exact = OsuApiSettings {
        user: Some("é".repeat(MAX_OSU_USER_BYTES / 2)),
        ..OsuApiSettings::default()
    };
    exact.validate().unwrap();

    let overflow = OsuApiSettings {
        user: Some(format!("{}a", "é".repeat(MAX_OSU_USER_BYTES / 2))),
        ..OsuApiSettings::default()
    };
    assert!(overflow.validate().unwrap_err().contains("257 UTF-8 bytes"));
}

#[test]
fn each_osu_profile_field_rejects_one_byte_or_digit_over_the_limit() {
    let cases = [
        (
            OsuApiSettings {
                client_id: Some(repeated_ascii(b'9', MAX_OSU_CLIENT_ID_DIGITS + 1)),
                ..OsuApiSettings::default()
            },
            "client id",
        ),
        (
            OsuApiSettings {
                user: Some(repeated_ascii(b'u', MAX_OSU_USER_BYTES + 1)),
                ..OsuApiSettings::default()
            },
            "osu! user",
        ),
        (
            OsuApiSettings {
                credential_target: Some(repeated_ascii(b't', MAX_OSU_CREDENTIAL_TARGET_BYTES + 1)),
                ..OsuApiSettings::default()
            },
            "credential target",
        ),
        (
            OsuApiSettings {
                last_connected_username: Some(repeated_ascii(
                    b'n',
                    MAX_OSU_CONNECTED_USERNAME_BYTES + 1,
                )),
                ..OsuApiSettings::default()
            },
            "connected username",
        ),
    ];

    for (settings, expected) in cases {
        let error = settings.validate().unwrap_err();
        assert!(error.contains(expected), "{error:?}");
    }

    let nondigit = OsuApiSettings {
        client_id: Some("123x".into()),
        ..OsuApiSettings::default()
    };
    assert!(nondigit.validate().unwrap_err().contains("number"));

    let numeric_u64_overflow = OsuApiSettings {
        client_id: Some("18446744073709551616".into()),
        ..OsuApiSettings::default()
    };
    assert!(numeric_u64_overflow
        .validate()
        .unwrap_err()
        .contains("number"));
}

#[test]
fn cleanup_count_is_checked_before_normalization_can_dedupe_or_drop_entries() {
    let mut duplicates = OsuApiSettings {
        credential_cleanup_targets: vec![
            "Clipline osu!:duplicate".into();
            MAX_OSU_CREDENTIAL_CLEANUP_TARGETS + 1
        ],
        ..OsuApiSettings::default()
    };

    duplicates.normalize();

    assert_eq!(
        duplicates.credential_cleanup_targets.len(),
        MAX_OSU_CREDENTIAL_CLEANUP_TARGETS + 1
    );
    assert!(duplicates.validate().unwrap_err().contains("at most 16"));
}

#[test]
fn aggregate_overflow_remains_rejected_after_normalization() {
    let mut settings = OsuApiSettings {
        credential_cleanup_targets: (0..MAX_OSU_CREDENTIAL_CLEANUP_TARGETS)
            .map(|index| cleanup_target(index, MAX_OSU_CREDENTIAL_TARGET_BYTES))
            .collect(),
        client_id: Some("1".into()),
        ..OsuApiSettings::default()
    };
    let before = settings.credential_cleanup_targets.clone();

    settings.normalize();

    assert_eq!(settings.credential_cleanup_targets, before);
    let error = settings.validate().unwrap_err();
    assert!(
        error.contains(&MAX_OSU_PROFILE_BYTES.to_string()),
        "{error}"
    );
}

#[test]
fn bounded_cleanup_targets_still_normalize_and_dedupe() {
    let mut settings = OsuApiSettings {
        credential_cleanup_targets: vec![
            " Clipline osu!:target-b ".into(),
            "Clipline osu!:target-a".into(),
            "Clipline osu!:target-b".into(),
            "  ".into(),
        ],
        ..OsuApiSettings::default()
    };

    settings.normalize();

    assert_eq!(
        settings.credential_cleanup_targets,
        ["Clipline osu!:target-a", "Clipline osu!:target-b"]
    );
    settings.validate().unwrap();
}

#[test]
fn active_credential_target_cannot_also_be_scheduled_for_cleanup() {
    let target = clipline_settings::osu::osu_credential_target("12345", "Dain");
    let settings = OsuApiSettings {
        client_id: Some("12345".into()),
        user: Some("Dain".into()),
        credential_target: Some(target.clone()),
        credential_cleanup_targets: vec![target],
        ..OsuApiSettings::default()
    };

    let error = settings.validate().unwrap_err();
    assert!(error.contains("active credential target"), "{error}");
}

#[test]
fn foreign_credential_targets_never_gain_cleanup_authority() {
    for settings in [
        OsuApiSettings {
            credential_target: Some("unrelated credential".into()),
            ..OsuApiSettings::default()
        },
        OsuApiSettings {
            credential_cleanup_targets: vec!["Clipline Cloud:account".into()],
            ..OsuApiSettings::default()
        },
    ] {
        let error = settings.validate().unwrap_err();
        assert!(error.contains("outside Clipline's osu! credential namespace"));
    }
}

#[test]
fn operation_targets_are_generation_owned_and_disjoint_from_legacy_user_text() {
    let generation = OsuAccountGeneration::new(9).unwrap();
    let operation = clipline_settings::osu::osu_credential_target_for_operation(
        generation,
        "01234567-89ab-cdef",
    )
    .unwrap();
    let crafted_legacy = clipline_settings::osu::osu_credential_target(
        "12345",
        "generation:9:operation:01234567-89ab-cdef",
    );

    assert_ne!(operation, crafted_legacy);
    assert!(
        clipline_settings::osu::is_osu_credential_target_for_generation(&operation, generation)
    );
    assert!(
        !clipline_settings::osu::is_osu_credential_target_for_generation(
            &operation,
            OsuAccountGeneration::new(8).unwrap()
        )
    );
    assert!(
        clipline_settings::osu::osu_credential_target_for_operation(generation, "bad:id").is_err()
    );
}

#[test]
fn legacy_json_defaults_and_new_json_persists_account_generation() {
    let legacy_osu = serde_json::json!({
        "client_id": "61835",
        "user": "3426414",
        "credential_target": "Clipline osu!:61835:3426414"
    });
    let mut legacy_document = serde_json::to_value(AppSettings::default()).unwrap();
    legacy_document["osu"] = legacy_osu;
    let document = AppSettings::load_from_object(legacy_document.as_object().unwrap());

    assert_eq!(
        document.osu.account_generation,
        OsuAccountGeneration::INITIAL
    );
    document.validate().unwrap();

    let encoded = serde_json::to_value(&document.osu).unwrap();
    assert_eq!(encoded["account_generation"], 1);
}

#[test]
fn account_generation_is_nonzero_and_fails_closed_at_exhaustion() {
    assert_eq!(
        OsuAccountGeneration::new(0),
        Err(OsuAccountGenerationError::Zero)
    );
    assert_eq!(OsuAccountGeneration::new(1).unwrap().get(), 1);
    assert_eq!(
        OsuAccountGeneration::new(u64::MAX - 1)
            .unwrap()
            .checked_next()
            .unwrap()
            .get(),
        u64::MAX
    );
    assert_eq!(
        OsuAccountGeneration::new(u64::MAX).unwrap().checked_next(),
        Err(OsuAccountGenerationError::Exhausted)
    );

    let zero_generation: OsuApiSettings =
        serde_json::from_str(r#"{"account_generation":0}"#).unwrap();
    assert!(zero_generation.validate().unwrap_err().contains("zero"));
    assert_eq!(
        zero_generation.account_generation.checked_next(),
        Err(OsuAccountGenerationError::Zero)
    );
}
