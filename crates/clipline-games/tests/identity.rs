use clipline_games::detection::GameWindowInfo;
use clipline_games::discovery::{
    validate_discovery_candidates, DetectedGameCandidate, DetectedGameSource,
    MAX_DISCOVERY_CATALOG_BYTES, MAX_DISCOVERY_TEXT_BYTES,
};
use clipline_games::identity::{
    built_in_id, CandidateGameIdentity, CustomGameIdentity, GameItemIdentity,
    GameItemIdentityError, GameWindowIdentityCatalog, InstalledGameIdentityCatalog,
    PluginGameIdentity, LEAGUE_OF_LEGENDS_ID, MAX_CANDIDATE_AUTHORITY_BYTES, OSU_ID,
};
use clipline_settings::{
    ProbeKind, ProbeRequestGeneration, ProbeSessionOwner, ProbeToken, SettingsAttachmentGeneration,
    SettingsForegroundGeneration, SettingsSessionGeneration,
};

fn token(kind: ProbeKind, request_generation: u64) -> ProbeToken {
    ProbeToken {
        owner: ProbeSessionOwner::new(
            SettingsSessionGeneration::new(7),
            SettingsAttachmentGeneration::new(11),
            SettingsForegroundGeneration::new(13),
        ),
        kind,
        request_generation: ProbeRequestGeneration::new(request_generation),
    }
}

fn installed_token(request_generation: u64) -> ProbeToken {
    token(ProbeKind::InstalledGames, request_generation)
}

fn window_token(request_generation: u64) -> ProbeToken {
    token(ProbeKind::GameWindows, request_generation)
}

fn candidate(
    process_path: Option<&str>,
    exe_name: &str,
    name: &str,
    id_hint: &str,
    title: &str,
) -> DetectedGameCandidate {
    DetectedGameCandidate {
        id_hint: id_hint.into(),
        name: name.into(),
        source: DetectedGameSource::RunningWindow,
        steam_app_id: None,
        install_dir: None,
        exe_name: exe_name.into(),
        process_path: process_path.map(str::to_owned),
        window_title: title.into(),
        icon: None,
        confidence: 90,
    }
}

fn window(process_id: u32, title: &str, exe_name: &str, exe_path: Option<&str>) -> GameWindowInfo {
    GameWindowInfo {
        title: title.into(),
        process_id,
        exe_name: exe_name.into(),
        exe_path: exe_path.map(str::to_owned),
    }
}

fn exact_length_unique_text(index: usize, length: usize) -> String {
    let prefix = format!("{index:08x}");
    assert!(prefix.len() <= length);
    let mut value = String::new();
    value.try_reserve_exact(length).unwrap();
    value.push_str(&prefix);
    value.extend(std::iter::repeat_n('x', length - prefix.len()));
    value
}

#[test]
fn built_in_custom_and_catalog_candidates_never_overlap() {
    assert!(clipline_settings::games::validate_custom_game_id(OSU_ID).is_err());
    assert!(clipline_settings::games::validate_custom_game_id(LEAGUE_OF_LEGENDS_ID).is_err());
    assert!(clipline_settings::games::validate_custom_game_id("custom-osu-123").is_ok());
    assert!(clipline_games::identity::GameIdentity::custom(OSU_ID)
        .plugin_id()
        .is_none());
    assert!(
        !clipline_games::identity::GameIdentity::custom("unknown").is_built_in_plugin("unknown")
    );
    assert_eq!(built_in_id(OSU_ID), Some(OSU_ID));
    for plugin in clipline_games::plugin::all() {
        assert!(built_in_id(plugin.id()).is_some());
        assert!(clipline_settings::games::validate_custom_game_id(plugin.id()).is_err());
    }

    let catalog = InstalledGameIdentityCatalog::build(
        installed_token(1),
        vec![candidate(None, "osu!.exe", "osu!", "window-osu", "osu!")],
    )
    .unwrap();
    let plugin = GameItemIdentity::Plugin(PluginGameIdentity::new(OSU_ID).unwrap());
    let custom = GameItemIdentity::Custom(CustomGameIdentity::new("custom-osu-123").unwrap());
    let discovered = GameItemIdentity::Candidate(catalog.identity_at(0).unwrap().clone());
    assert_ne!(plugin, custom);
    assert_ne!(plugin, discovered);
    assert_ne!(custom, discovered);
}

#[test]
fn plugin_and_custom_constructors_enforce_the_persisted_namespaces() {
    assert_eq!(PluginGameIdentity::new(OSU_ID).unwrap().as_str(), OSU_ID);
    assert_eq!(
        PluginGameIdentity::new("custom-osu").unwrap_err(),
        GameItemIdentityError::UnknownPlugin("custom-osu".into())
    );
    assert_eq!(
        CustomGameIdentity::new(OSU_ID).unwrap_err(),
        GameItemIdentityError::InvalidCustomId(
            "custom game id \"osu\" is reserved for a built-in game".into()
        )
    );
    assert!(CustomGameIdentity::new("custom-valid-game").is_ok());
}

#[test]
fn installed_catalog_uses_complete_canonical_authority_and_hides_source_text() {
    let first = candidate(
        Some(" C:/Games/Example/Game.EXE/ "),
        "GAME.EXE",
        "Example",
        "window-example",
        "First Match",
    );
    let same_canonical = candidate(
        Some("c:\\games\\example\\game.exe"),
        "game.exe",
        "example",
        "WINDOW-EXAMPLE",
        "first match",
    );
    let same_path_and_exe_different_title = candidate(
        Some("c:\\games\\example\\game.exe"),
        "game.exe",
        "Example",
        "window-example-2",
        "Second Match",
    );

    let first_catalog =
        InstalledGameIdentityCatalog::build(installed_token(2), vec![first]).unwrap();
    let same_catalog =
        InstalledGameIdentityCatalog::build(installed_token(2), vec![same_canonical]).unwrap();
    let distinct_catalog = InstalledGameIdentityCatalog::build(
        installed_token(2),
        vec![same_path_and_exe_different_title],
    )
    .unwrap();

    let first_id = first_catalog.identity_at(0).unwrap();
    assert_eq!(first_id, same_catalog.identity_at(0).unwrap());
    assert_ne!(first_id, distinct_catalog.identity_at(0).unwrap());
    assert!(!first_id.opaque_id().contains("games"));
    assert!(!first_id.opaque_id().contains("example"));
    assert_eq!(first_catalog.resolve(first_id).unwrap().name, "Example");
    assert_eq!(first_catalog.resolve_index(first_id).unwrap(), 0);
}

#[test]
fn window_catalog_uses_real_window_info_and_distinguishes_process_and_title() {
    let rows = vec![
        window(100, "First Match", "game.exe", Some("c:\\games\\game.exe")),
        window(100, "Second Match", "game.exe", Some("c:\\games\\game.exe")),
        window(101, "First Match", "game.exe", Some("c:\\games\\game.exe")),
    ];
    let catalog = GameWindowIdentityCatalog::build(window_token(4), rows.clone()).unwrap();
    assert_eq!(catalog.len(), 3);
    assert_ne!(catalog.identity_at(0), catalog.identity_at(1));
    assert_ne!(catalog.identity_at(0), catalog.identity_at(2));
    for (index, (identity, source)) in catalog.iter().enumerate() {
        assert_eq!(source, &rows[index]);
        assert_eq!(catalog.resolve_index(identity).unwrap(), index);
        assert_eq!(catalog.resolve(identity).unwrap(), source);
    }
}

#[test]
fn catalog_membership_rejects_superseded_and_forged_handles() {
    let source = candidate(Some("c:\\game.exe"), "game.exe", "Game", "game", "Game");
    let current =
        InstalledGameIdentityCatalog::build(installed_token(5), vec![source.clone()]).unwrap();
    let superseding =
        InstalledGameIdentityCatalog::build(installed_token(6), vec![source]).unwrap();
    let current_id = current.identity_at(0).unwrap();
    let stale_id = superseding.identity_at(0).unwrap();
    assert_eq!(current_id.opaque_id(), stale_id.opaque_id());
    assert_ne!(current_id, stale_id);
    assert_eq!(
        current.resolve(stale_id).unwrap_err(),
        GameItemIdentityError::CandidateTokenMismatch {
            expected: installed_token(5),
            actual: installed_token(6),
        }
    );

    let mut forged = serde_json::to_value(GameItemIdentity::Candidate(current_id.clone())).unwrap();
    forged["value"]["opaque_id"] =
        serde_json::Value::String(format!("candidate-v2-{}", "0".repeat(64)));
    let GameItemIdentity::Candidate(forged) =
        serde_json::from_value::<GameItemIdentity>(forged).unwrap()
    else {
        panic!("candidate identity should deserialize");
    };
    assert_eq!(
        current.resolve(&forged).unwrap_err(),
        GameItemIdentityError::CandidateNotInCatalog
    );
}

#[test]
fn duplicate_complete_authority_fails_the_whole_catalog() {
    let source = candidate(Some("c:\\game.exe"), "game.exe", "Game", "game", "Game");
    assert_eq!(
        InstalledGameIdentityCatalog::build(installed_token(7), vec![source.clone(), source],)
            .unwrap_err(),
        GameItemIdentityError::DuplicateCandidateAuthority {
            first_index: 0,
            duplicate_index: 1,
        }
    );

    let source = window(10, "Game", "game.exe", Some("c:\\game.exe"));
    assert_eq!(
        GameWindowIdentityCatalog::build(window_token(7), vec![source.clone(), source])
            .unwrap_err(),
        GameItemIdentityError::DuplicateCandidateAuthority {
            first_index: 0,
            duplicate_index: 1,
        }
    );
}

#[test]
fn catalog_builders_reject_wrong_probe_kinds_and_oversized_authority() {
    let source = candidate(None, "game.exe", "Game", "game", "Game");
    assert_eq!(
        InstalledGameIdentityCatalog::build(window_token(1), vec![source.clone()]).unwrap_err(),
        GameItemIdentityError::CandidateProbeKindMismatch {
            expected: ProbeKind::InstalledGames,
            actual: ProbeKind::GameWindows,
        }
    );
    assert_eq!(
        GameWindowIdentityCatalog::build(
            installed_token(1),
            vec![window(1, "Game", "game.exe", None)]
        )
        .unwrap_err(),
        GameItemIdentityError::CandidateProbeKindMismatch {
            expected: ProbeKind::GameWindows,
            actual: ProbeKind::InstalledGames,
        }
    );

    let oversized = "x".repeat(MAX_CANDIDATE_AUTHORITY_BYTES + 1);
    assert!(matches!(
        InstalledGameIdentityCatalog::build(
            installed_token(1),
            vec![candidate(
                Some(&oversized),
                "game.exe",
                "Game",
                "game",
                "Game"
            )],
        )
        .unwrap_err(),
        GameItemIdentityError::InvalidCandidateCatalog(_)
    ));
}

#[test]
fn installed_identity_framing_accepts_the_exact_source_catalog_maximum() {
    assert_eq!(MAX_DISCOVERY_CATALOG_BYTES % MAX_DISCOVERY_TEXT_BYTES, 0);
    let row_count = MAX_DISCOVERY_CATALOG_BYTES / MAX_DISCOVERY_TEXT_BYTES;
    let exact: Vec<_> = (0..row_count)
        .map(|index| {
            candidate(
                None,
                "",
                "",
                &exact_length_unique_text(index, MAX_DISCOVERY_TEXT_BYTES),
                "",
            )
        })
        .collect();
    validate_discovery_candidates(&exact).expect("Task 5 exact-max source payload is valid");
    let catalog = InstalledGameIdentityCatalog::build(installed_token(30), exact)
        .expect("derived identity framing must not consume the source payload budget");
    assert_eq!(catalog.len(), row_count);

    let mut one_over: Vec<_> = (0..row_count)
        .map(|index| {
            candidate(
                None,
                "",
                "",
                &exact_length_unique_text(index, MAX_DISCOVERY_TEXT_BYTES),
                "",
            )
        })
        .collect();
    one_over.push(candidate(None, "", "", "z", ""));
    assert!(validate_discovery_candidates(&one_over).is_err());
    assert!(matches!(
        InstalledGameIdentityCatalog::build(installed_token(31), one_over).unwrap_err(),
        GameItemIdentityError::InvalidCandidateCatalog(_)
    ));
}

#[test]
fn window_identity_framing_accepts_the_exact_source_catalog_maximum() {
    let row_count = MAX_DISCOVERY_CATALOG_BYTES / MAX_DISCOVERY_TEXT_BYTES;
    let exact: Vec<_> = (0..row_count)
        .map(|index| {
            window(
                u32::try_from(index + 1).unwrap(),
                &exact_length_unique_text(index, MAX_DISCOVERY_TEXT_BYTES),
                "",
                None,
            )
        })
        .collect();
    let catalog = GameWindowIdentityCatalog::build(window_token(32), exact)
        .expect("derived identity framing must not consume the source payload budget");
    assert_eq!(catalog.len(), row_count);

    let mut one_over: Vec<_> = (0..row_count)
        .map(|index| {
            window(
                u32::try_from(index + 1).unwrap(),
                &exact_length_unique_text(index, MAX_DISCOVERY_TEXT_BYTES),
                "",
                None,
            )
        })
        .collect();
    one_over.push(window(1000, "z", "", None));
    assert!(matches!(
        GameWindowIdentityCatalog::build(window_token(33), one_over).unwrap_err(),
        GameItemIdentityError::InvalidCandidateCatalog(_)
    ));
}

#[test]
fn item_identities_round_trip_but_only_catalogs_grant_membership() {
    let installed = InstalledGameIdentityCatalog::build(
        installed_token(9),
        vec![candidate(
            Some("c:\\osu!.exe"),
            "osu!.exe",
            "osu!",
            "osu",
            "osu!",
        )],
    )
    .unwrap();
    let windows = GameWindowIdentityCatalog::build(
        window_token(10),
        vec![window(42, "osu!", "osu!.exe", Some("c:\\osu!.exe"))],
    )
    .unwrap();
    let values = [
        GameItemIdentity::Plugin(PluginGameIdentity::new(OSU_ID).unwrap()),
        GameItemIdentity::Custom(CustomGameIdentity::new("custom-osu-123").unwrap()),
        GameItemIdentity::Candidate(installed.identity_at(0).unwrap().clone()),
        GameItemIdentity::Candidate(windows.identity_at(0).unwrap().clone()),
    ];

    for value in values {
        let json = serde_json::to_string(&value).unwrap();
        assert_eq!(
            serde_json::from_str::<GameItemIdentity>(&json).unwrap(),
            value
        );
    }
    assert!(serde_json::from_str::<GameItemIdentity>(
        r#"{"kind":"plugin","value":"not-a-plugin"}"#
    )
    .is_err());
    assert!(
        serde_json::from_str::<GameItemIdentity>(r#"{"kind":"custom","value":"osu"}"#).is_err()
    );
}

#[test]
fn candidate_deserialization_rejects_every_unrelated_probe_kind() {
    let catalog = InstalledGameIdentityCatalog::build(
        installed_token(12),
        vec![candidate(
            Some("c:\\game.exe"),
            "game.exe",
            "Game",
            "game",
            "Game",
        )],
    )
    .unwrap();
    let template = serde_json::to_value(GameItemIdentity::Candidate(
        catalog.identity_at(0).unwrap().clone(),
    ))
    .unwrap();

    for kind in [
        "displays",
        "audio_endpoints",
        "encoders",
        "game_plugins",
        "storage",
        "playback_capabilities",
    ] {
        let mut json = template.clone();
        json["value"]["token"]["kind"] = serde_json::Value::String(kind.into());
        let error = serde_json::from_value::<GameItemIdentity>(json).unwrap_err();
        assert!(
            error.to_string().contains("does not support"),
            "unexpected error for {kind}: {error}"
        );
    }
}

#[test]
fn candidate_identity_has_no_public_direct_constructor() {
    fn accepts_identity(_: &CandidateGameIdentity) {}
    let catalog = GameWindowIdentityCatalog::build(
        window_token(20),
        vec![window(10, "Game", "game.exe", Some("c:\\game.exe"))],
    )
    .unwrap();
    accepts_identity(catalog.identity_at(0).unwrap());
}
