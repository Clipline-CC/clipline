use super::*;
use clipline_capture::windows::CapturableWindow;
use clipline_test_utils::TestDir;

fn window(
    handle: isize,
    title: &str,
    exe_name: &str,
    exe_path: Option<&str>,
) -> CapturableWindow {
    CapturableWindow {
        handle,
        title: title.into(),
        process_id: handle as u32,
        exe_name: exe_name.into(),
        exe_path: exe_path.map(str::to_string),
    }
}

#[test]
fn manifest_rejects_unsupported_schema_versions() {
    let json = r#"{
      "schema_version": 2,
      "id": "league_of_legends",
      "name": "League of Legends",
      "summary": "Auto-records full matches when the in-game window is active.",
      "default_enabled": true,
      "default_recording_mode": "full_session",
      "window_match": { "exe_name": "League of Legends.exe", "selection": "longest_title" },
      "event_source": "league_live_client"
    }"#;

    let err = GameProfileManifest::from_json(json).unwrap_err();

    assert!(err.contains("unsupported game profile schema"), "{err}");
}

#[test]
fn unsupported_event_source_names_are_rejected() {
    let json = r#"{
      "schema_version": 1,
      "id": "future_game",
      "name": "Future Game",
      "summary": "Future game profile.",
      "default_enabled": true,
      "default_recording_mode": "full_session",
      "window_match": { "exe_name": "Future.exe", "selection": "longest_title" },
      "event_source": "future_live_client"
    }"#;

    let err = GameProfileManifest::from_json(json).unwrap_err();

    assert!(err.contains("unsupported game event source"), "{err}");
}

#[test]
fn declarative_league_matcher_preserves_longest_title_behavior() {
    let manifest = league_profile_manifest();
    let windows = vec![
        window(1, "League of Legends", "LeagueClientUx.exe", None),
        window(2, "League", "League of Legends.exe", None),
        window(
            3,
            "League of Legends (TM) Client",
            "League of Legends.exe",
            None,
        ),
    ];

    let matched = manifest.match_window(&windows).expect("game window");

    assert_eq!(matched.handle, 3);
    assert_eq!(matched.exe_name, "League of Legends.exe");
}

#[test]
fn league_profile_has_no_install_state_but_keeps_presentation() {
    let profile = all()
        .iter()
        .find(|profile| profile.id() == LEAGUE_OF_LEGENDS_ID)
        .expect("league profile");
    let info = profile.info();

    assert_eq!(info.id, LEAGUE_OF_LEGENDS_ID);
    assert_eq!(info.name, "League of Legends");
    assert!(info.default_enabled);
    assert_eq!(info.default_recording_mode, GameRecordingMode::FullSession);
    assert!(info.event_markers);
    assert!(info
        .icon
        .as_deref()
        .is_some_and(|icon| icon.starts_with("data:image/png;base64,")));

    let presentation = info.presentation.expect("league presentation");
    assert_eq!(
        presentation
            .pointer("/event_rail/title")
            .and_then(serde_json::Value::as_str),
        Some("Match events")
    );
    assert_eq!(
        presentation
            .pointer("/metadata_panel/fields/0/asset_provider")
            .and_then(serde_json::Value::as_str),
        Some("riot_data_dragon_champion_square")
    );
    assert!(presentation
        .pointer("/marker_kinds/ChampionKill/icon")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|icon| icon.starts_with("data:image/png;base64,")));
    assert!(presentation
        .pointer("/marker_kinds/ChampionAssist/icon")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|icon| icon.starts_with("data:image/png;base64,")));
    assert!(presentation
        .pointer("/event_rail/icons/ChampionKill")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|icon| icon.starts_with("data:image/png;base64,")));
    assert!(presentation
        .pointer("/event_rail/icons/HeraldKill")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|icon| icon.starts_with("data:image/png;base64,")));
    assert!(presentation
        .pointer("/event_rail/actor_icons/1/asset")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|icon| icon.starts_with("data:image/png;base64,")));
}

#[test]
fn osu_profile_is_registered_without_live_event_source() {
    let profile = all()
        .iter()
        .find(|profile| profile.id() == "osu")
        .expect("osu profile");
    let info = profile.info();

    assert_eq!(plugin_id_for_game_id(GameId::Osu), "osu");
    assert_eq!(display_name_for_game_id(GameId::Osu), "osu!");
    assert_eq!(info.name, "osu!");
    assert_eq!(
        info.summary,
        "Auto-records osu!standard sessions and enriches saved sessions with submitted plays."
    );
    assert!(info.default_enabled);
    assert_eq!(info.default_recording_mode, GameRecordingMode::FullSession);
    assert!(!info.event_markers);
    assert!(!has_event_source(Some("osu")));
    assert!(info
        .icon
        .as_deref()
        .is_some_and(|icon| icon.starts_with("data:image/png;base64,")));

    let presentation = info.presentation.expect("osu presentation");
    assert_eq!(
        presentation
            .pointer("/play_blocks/enabled")
            .and_then(serde_json::Value::as_bool),
        Some(true)
    );
    assert_eq!(
        presentation
            .pointer("/play_rail/title")
            .and_then(serde_json::Value::as_str),
        Some("Set plays")
    );
    assert_eq!(
        presentation
            .pointer("/play_rail/empty")
            .and_then(serde_json::Value::as_str),
        Some("No osu! plays loaded yet. Add osu! API credentials in osu! settings to fetch submitted plays.")
    );
    assert_eq!(
        presentation
            .pointer("/gallery/summary")
            .and_then(serde_json::Value::as_str),
        Some("osu_set_plays")
    );

    let windows = vec![
        window(1, "osu!", "osu!.exe", None),
        window(
            2,
            "osu!cuttingedge b20260624",
            "osu!.exe",
            Some(r"C:\Users\dain\AppData\Roaming\osu!\osu!.exe"),
        ),
        window(
            3,
            "osu! - camellia - exit this earth's atomosphere",
            "osu!.exe",
            None,
        ),
    ];
    let matched = profile.match_window(&windows).expect("osu window");
    assert_eq!(matched.handle, 3);
    assert_eq!(matched.exe_name, "osu!.exe");
}

#[test]
fn osu_profile_accepts_cutting_edge_build_title() {
    let profile = all()
        .iter()
        .find(|profile| profile.id() == OSU_ID)
        .expect("osu profile");
    let windows = vec![window(
        1,
        "osu!cuttingedge b20260624",
        "osu!.exe",
        Some(r"C:\Users\dain\AppData\Roaming\osu!\osu!.exe"),
    )];

    let matched = profile
        .match_window(&windows)
        .expect("osu cutting-edge gameplay window");

    assert_eq!(matched.handle, 1);
}

#[test]
fn osu_profile_accepts_stable_play_title_with_extra_spacing() {
    let profile = all()
        .iter()
        .find(|profile| profile.id() == OSU_ID)
        .expect("osu profile");
    let windows = vec![window(
        1,
        "osu!  - ginkiha - EOS [Lycoris]",
        "osu!.exe",
        Some(r"C:\Users\dain\AppData\Roaming\osu!\osu!.exe"),
    )];

    let matched = profile
        .match_window(&windows)
        .expect("osu stable gameplay window");

    assert_eq!(matched.handle, 1);
}

#[test]
fn osu_profile_accepts_stable_idle_title() {
    let profile = all()
        .iter()
        .find(|profile| profile.id() == OSU_ID)
        .expect("osu profile");
    let windows = vec![window(
        1,
        "osu!",
        "osu!.exe",
        Some(r"C:\Users\dain\AppData\Roaming\osu!\osu!.exe"),
    )];

    let matched = profile
        .match_window(&windows)
        .expect("osu stable idle window");

    assert_eq!(matched.handle, 1);
}

#[test]
fn osu_profile_ignores_updater_client_windows() {
    let profile = all()
        .iter()
        .find(|profile| profile.id() == OSU_ID)
        .expect("osu profile");

    for title in [
        "osu! updater",
        "osu! cutting edge",
        "osu! update available",
        "osu! updating",
    ] {
        let windows = vec![window(
            1,
            title,
            "osu!.exe",
            Some(r"C:\Users\dain\AppData\Local\osu!\osu!.exe"),
        )];

        assert!(
            profile.match_window(&windows).is_none(),
            "{title:?} should not be treated as an osu! game window"
        );
    }
}

#[test]
fn profile_records_and_immutable_catalog_data_are_cached() {
    let first_profiles = all();
    let second_profiles = all();
    assert_eq!(first_profiles.as_ptr(), second_profiles.as_ptr());

    let first_base = catalog_base();
    let second_base = catalog_base();
    assert_eq!(first_base.as_ptr(), second_base.as_ptr());
    let first_catalog = catalog();
    let second_catalog = catalog();
    assert_ne!(first_catalog.as_ptr(), second_catalog.as_ptr());
    assert!(first_catalog[0]
        .presentation
        .as_ref()
        .and_then(|presentation| presentation.pointer("/event_rail/icons/ChampionKill"))
        .and_then(serde_json::Value::as_str)
        .is_some_and(|icon| icon.starts_with("data:image/png;base64,")));
    assert!(first_catalog[0]
        .presentation
        .as_ref()
        .and_then(|presentation| presentation.pointer("/event_rail/actor_icons/0/asset"))
        .and_then(serde_json::Value::as_str)
        .is_some_and(|icon| icon.starts_with("data:image/png;base64,")));
}

#[test]
fn extracted_icon_resolver_observes_a_file_created_after_a_missing_read() {
    let dir = TestDir::new("clipline-game-plugins", "late-extracted-icon");
    let cache = dir.path().join("future-game.png");

    assert!(extracted_icon_data_url(&cache).is_none());
    std::fs::write(&cache, [0x89, b'P', b'N', b'G', 1, 2, 3, 4]).unwrap();

    assert!(extracted_icon_data_url(&cache)
        .as_deref()
        .is_some_and(|icon| icon.starts_with("data:image/png;base64,")));
}

#[test]
fn league_profile_declares_categories_for_all_live_client_kinds() {
    // The review filters key on marker categories, so every EventKind the
    // Live Client poller can emit needs a deliberate category — otherwise
    // it silently degrades to "info" and disappears from both surfaces.
    let manifest = league_profile_manifest();
    let presentation = manifest.presentation.expect("presentation");
    let kinds = [
        ("GameStart", "info"),
        ("MinionsSpawning", "info"),
        ("FirstBrick", "info"),
        ("TurretKilled", "structure"),
        ("InhibKilled", "structure"),
        ("DragonKill", "objective"),
        ("HeraldKill", "objective"),
        ("BaronKill", "objective"),
        ("ChampionKill", "kill"),
        ("ChampionAssist", "assist"),
        ("ChampionDeath", "death"),
        ("Multikill", "spree"),
        ("Ace", "spree"),
        ("FirstBlood", "spree"),
        ("GameEnd", "info"),
    ];
    for (kind, category) in kinds {
        assert_eq!(
            presentation
                .pointer(&format!("/marker_kinds/{kind}/category"))
                .and_then(serde_json::Value::as_str),
            Some(category),
            "League profile should categorize {kind}"
        );
    }
}

#[test]
fn league_profile_declares_data_dragon_portrait_provider() {
    let manifest = league_profile_manifest();
    let presentation = manifest.presentation.expect("presentation");

    assert_eq!(
        presentation
            .pointer("/data_dragon/version")
            .and_then(serde_json::Value::as_str),
        Some("16.13.1")
    );
    assert_eq!(
        presentation
            .pointer("/metadata_panel/fields/0/asset_provider")
            .and_then(serde_json::Value::as_str),
        Some("riot_data_dragon_champion_square")
    );
    assert_eq!(
        presentation
            .pointer("/metadata_panel/fields/0/asset_aliases/wukong")
            .and_then(serde_json::Value::as_str),
        Some("MonkeyKing")
    );
}

#[test]
fn game_id_bridge_keeps_existing_ids() {
    assert_eq!(
        plugin_id_for_game_id(GameId::LeagueOfLegends),
        LEAGUE_OF_LEGENDS_ID
    );
    assert_eq!(plugin_id_for_game_id(GameId::Valorant), "valorant");
    assert_eq!(plugin_id_for_game_id(GameId::Cs2), "cs2");
    assert_eq!(plugin_id_for_game_id(GameId::Osu), "osu");
}
